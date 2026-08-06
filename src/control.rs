use std::{
    collections::BTreeMap,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, bail, eyre};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

use crate::{
    cli::*,
    daemon::{ApiEnvelope, ApiPayload, ApiRequest, ApiResponse, DownloadItem, Snapshot},
    paths::AppPaths,
    routing::{DownloadRoutingRule, match_rule},
    state::{ManualOrScheduled, TorrentStreamingMode},
    units,
    webhook::WebhookPingMode,
};

pub fn is_control_command(command: &Commands) -> bool {
    !matches!(
        command,
        Commands::Ui | Commands::Daemon | Commands::Service { .. }
    )
}

pub fn report_error(cli: &Cli, error: &color_eyre::Report) -> i32 {
    let message = error.to_string();
    let (code, exit) = if message.contains("partial batch") {
        ("partial_batch_failure", 10)
    } else if message.contains("timed out") {
        ("timeout", 7)
    } else if message.contains("connect to daemon") {
        ("daemon_unavailable", 3)
    } else if message.contains("not found") {
        ("not_found", 4)
    } else if message.contains("ambiguous") {
        ("ambiguous_selector", 5)
    } else if message.contains("path_not_permitted") || message.contains("permission") {
        ("permission_denied", 6)
    } else if message.contains("unsupported") {
        ("unsupported", 9)
    } else if message.contains("invalid")
        || message.contains("must ")
        || message.contains("requires --yes")
    {
        ("invalid_input", 2)
    } else {
        ("operation_failed", 8)
    };
    if cli.json || matches!(cli.format, Some(OutputFormat::Json | OutputFormat::Jsonl)) {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "api_version":"1",
                "ok":false,
                "error":{"code":code,"message":message,"retryable":matches!(exit,3|7|8)}
            }))
            .expect("error response is serializable")
        );
    } else {
        eprintln!("AriaTUI command failed: {message}");
    }
    exit
}

pub async fn run(cli: &Cli, paths: &AppPaths, command: Commands) -> Result<()> {
    let format = if cli.json {
        OutputFormat::Json
    } else {
        cli.format.unwrap_or_else(|| {
            if io::stdout().is_terminal() {
                OutputFormat::Human
            } else {
                OutputFormat::Json
            }
        })
    };
    let request_timeout = parse_duration(&cli.timeout)?;
    let client = Client::new(
        discover_socket(cli.socket.as_deref(), paths),
        request_timeout,
        cli.request_id.clone(),
    );
    match command {
        Commands::Status { watch, interval } => {
            let interval = parse_duration(&interval)?;
            loop {
                let snapshot = client.snapshot().await?;
                emit(
                    format,
                    "status",
                    status_value(&snapshot),
                    human_status(&snapshot),
                )?;
                if !watch {
                    break;
                }
                tokio::time::sleep(interval).await;
            }
        }
        Commands::Doctor => doctor(format, &client, paths).await?,
        Commands::Capabilities => emit(
            format,
            "capabilities",
            capabilities(),
            "AriaTUI CLI API v1: structured output and all primary daemon controls are available"
                .into(),
        )?,
        Commands::Events { interval, count } => {
            events(format, &client, parse_duration(&interval)?, count).await?
        }
        Commands::Batch {
            file,
            stop_on_error,
        } => batch(format, &client, &file, stop_on_error).await?,
        Commands::Api { command } => match command {
            ApiCommands::Request { payload } => {
                let request: ApiRequest = serde_json::from_str(&payload)?;
                let response = client.request(request).await?;
                emit(
                    format,
                    "api.request",
                    serde_json::to_value(response)?,
                    "Raw API request completed.".into(),
                )?;
            }
        },
        Commands::Schema { command } => emit(
            format,
            "schema",
            schema(command.as_deref()),
            "Use --json to read the command and output contract.".into(),
        )?,
        Commands::Download { command } => download(format, &client, command).await?,
        Commands::Queue { command } => queue(format, &client, command).await?,
        Commands::History { command } => history(format, &client, command).await?,
        Commands::Speed { command } => speed(format, &client, command).await?,
        Commands::Schedule { command } => schedule(format, &client, command).await?,
        Commands::Route { command } => route(format, &client, command).await?,
        Commands::Torrent { command } => torrent(format, &client, command).await?,
        Commands::Web { command } => web(format, &client, command).await?,
        Commands::Webhook { command } => webhook(format, &client, command).await?,
        Commands::Config { command } => config(format, &client, command).await?,
        _ => bail!("command is not a control command"),
    }
    Ok(())
}

async fn events(
    format: OutputFormat,
    client: &Client,
    interval: Duration,
    count: Option<usize>,
) -> Result<()> {
    let mut previous = client.snapshot().await?;
    let mut emitted = 0usize;
    loop {
        tokio::time::sleep(interval).await;
        let next = client.snapshot().await?;
        for item in next
            .current_downloads
            .iter()
            .chain(next.history_downloads.iter())
        {
            let old = previous
                .current_downloads
                .iter()
                .chain(previous.history_downloads.iter())
                .find(|old| old.gid == item.gid);
            if old != Some(item) {
                let kind = if old.is_none() {
                    "download.added"
                } else {
                    match item.status {
                        crate::daemon::DownloadStatus::Complete => "download.completed",
                        crate::daemon::DownloadStatus::Error => "download.failed",
                        _ => "download.changed",
                    }
                };
                emit(
                    if format == OutputFormat::Json {
                        OutputFormat::Jsonl
                    } else {
                        format
                    },
                    "events",
                    json!({"type":kind,"at":chrono::Utc::now().to_rfc3339(),"download":item}),
                    format!("{kind}: {} ({})", item.name, item.gid),
                )?;
                emitted += 1;
                if count.is_some_and(|limit| emitted >= limit) {
                    return Ok(());
                }
            }
        }
        previous = next;
    }
}

async fn batch(format: OutputFormat, client: &Client, file: &Path, stop: bool) -> Result<()> {
    let contents = if file == Path::new("-") {
        let mut value = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut value)?;
        value
    } else {
        std::fs::read_to_string(file)?
    };
    let requests: Vec<ApiRequest> = serde_json::from_str(&contents)
        .map_err(|error| eyre!("batch input must be a JSON array of API requests: {error}"))?;
    let mut results = Vec::with_capacity(requests.len());
    for (index, request) in requests.into_iter().enumerate() {
        match client.request(request).await {
            Ok(response) => results.push(json!({"index":index,"ok":true,"response":response})),
            Err(error) => {
                results
                    .push(json!({"index":index,"ok":false,"error":{"message":error.to_string()}}));
                if stop {
                    break;
                }
            }
        }
    }
    let failures = results.iter().filter(|v| v["ok"] == false).count();
    emit(
        format,
        "batch",
        json!({"results":results,"failures":failures}),
        format!("Batch completed with {failures} failure(s)."),
    )?;
    if failures > 0 {
        bail!("partial batch failure: {failures} operation(s) failed")
    }
    Ok(())
}

#[derive(Clone)]
struct Client {
    socket: PathBuf,
    timeout: Duration,
    request_id: Option<String>,
}

impl Client {
    fn new(socket: PathBuf, timeout: Duration, request_id: Option<String>) -> Self {
        Self {
            socket,
            timeout,
            request_id,
        }
    }
    async fn request(&self, request: ApiRequest) -> Result<ApiResponse> {
        let mut stream = timeout(self.timeout, UnixStream::connect(&self.socket))
            .await
            .map_err(|_| {
                eyre!(
                    "timed out connecting to daemon at {}",
                    self.socket.display()
                )
            })?
            .map_err(|error| {
                eyre!(
                    "failed to connect to daemon at {}: {error}",
                    self.socket.display()
                )
            })?;
        let id = self.request_id.clone().unwrap_or_else(|| {
            format!(
                "cli-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_millis()
            )
        });
        let bytes = serde_json::to_vec(&ApiEnvelope { id, request })?;
        stream.write_all(&bytes).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;
        let mut line = String::new();
        timeout(self.timeout, BufReader::new(stream).read_line(&mut line))
            .await
            .map_err(|_| eyre!("timed out waiting for daemon response"))??;
        let response: ApiResponse = serde_json::from_str(&line)?;
        if !response.ok {
            bail!(
                "{}",
                response
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "daemon request failed".into())
            );
        }
        Ok(response)
    }
    async fn snapshot(&self) -> Result<Snapshot> {
        self.request(ApiRequest::GetSnapshot)
            .await?
            .result
            .ok_or_else(|| eyre!("daemon returned no snapshot"))
    }
    async fn mutate(&self, request: ApiRequest) -> Result<ApiResponse> {
        self.request(request).await
    }
}

fn discover_socket(explicit: Option<&Path>, paths: &AppPaths) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if paths.socket_path.exists() {
        return paths.socket_path.clone();
    }
    let system = PathBuf::from("/run/ariatui/daemon.sock");
    if system.exists() {
        system
    } else {
        paths.socket_path.clone()
    }
}

async fn doctor(format: OutputFormat, client: &Client, paths: &AppPaths) -> Result<()> {
    let socket_exists = client.socket.exists();
    let probe = client.snapshot().await;
    let value = json!({"socket": client.socket, "socket_exists": socket_exists, "config_path": paths.config_file, "state_path": paths.state_file, "daemon_reachable": probe.is_ok(), "aria2": probe.as_ref().ok().map(|s| &s.aria2_status), "warnings": probe.as_ref().ok().map(|s| &s.warnings)});
    let human = match probe {
        Ok(s) => format!(
            "OK: daemon reachable at {}\nOK: aria2 lifecycle is {:?}\nWarnings: {}",
            client.socket.display(),
            s.aria2_status.lifecycle,
            if s.warnings.is_empty() {
                "none".into()
            } else {
                s.warnings.join("; ")
            }
        ),
        Err(e) => format!("FAIL: {e}\nSocket checked: {}", client.socket.display()),
    };
    emit(format, "doctor", value, human)
}

async fn download(format: OutputFormat, client: &Client, command: DownloadCommands) -> Result<()> {
    match command {
        DownloadCommands::Add(args) => {
            let options = add_options(&args)?;
            if args.dry_run {
                return emit(
                    format,
                    "download.add",
                    json!({"dry_run":true,"url":args.url,"directory":args.dir,"output_name":args.output_name,"options":options}),
                    "Download request is valid (dry run; nothing added).".into(),
                );
            }
            let directory = args.dir.as_deref().map(expand_cli_path).transpose()?;
            let response = client
                .mutate(ApiRequest::AddDownload {
                    url: args.url,
                    filename: args.output_name,
                    directory,
                    options,
                    idempotency_key: args.idempotency_key,
                })
                .await?;
            let data = match response.payload {
                Some(ApiPayload::Download { download, created }) => {
                    json!({"download":download,"created":created})
                }
                _ => json!({"created":true,"snapshot":response.result}),
            };
            emit(
                format,
                "download.add",
                data,
                "Download added successfully.".into(),
            )
        }
        DownloadCommands::Resolve { url } => {
            let response = client.request(ApiRequest::ResolveHttpUrl { url }).await?;
            let data = match response.payload {
                Some(ApiPayload::ResolvedHttpUrl(v)) => serde_json::to_value(v)?,
                _ => Value::Null,
            };
            emit(
                format,
                "download.resolve",
                data,
                "URL resolved successfully.".into(),
            )
        }
        DownloadCommands::List(args) => {
            let snapshot = client.snapshot().await?;
            let source = if args.history {
                &snapshot.history_downloads
            } else {
                &snapshot.current_downloads
            };
            let items = filter_items(source, &args);
            emit_items(format, "download.list", items)
        }
        DownloadCommands::Show(selector) => show_item(format, client, &selector, false).await,
        DownloadCommands::Files(selector) => files(format, client, &selector).await,
        DownloadCommands::Pause { selector, force } => {
            mutate_selected(
                format,
                client,
                &selector,
                |gid| ApiRequest::Pause { gid, force },
                "paused",
            )
            .await
        }
        DownloadCommands::Resume(selector) => {
            mutate_selected(
                format,
                client,
                &selector,
                |gid| ApiRequest::Resume { gid },
                "resumed",
            )
            .await
        }
        DownloadCommands::Cancel {
            selector,
            delete_files,
            yes,
        } => {
            if delete_files && !yes {
                bail!("--delete-files requires --yes");
            }
            mutate_selected(
                format,
                client,
                &selector,
                |gid| ApiRequest::Cancel { gid, delete_files },
                if delete_files {
                    "cancelled and files deleted"
                } else {
                    "cancelled; partial files kept"
                },
            )
            .await
        }
        DownloadCommands::Wait {
            selector,
            until,
            wait_timeout,
            interval,
        } => {
            wait_download(
                format,
                client,
                selector,
                &until,
                parse_duration(&wait_timeout)?,
                parse_duration(&interval)?,
            )
            .await
        }
        DownloadCommands::Move { selector, offset } => {
            mutate_selected(
                format,
                client,
                &selector,
                |gid| ApiRequest::ChangePosition { gid, offset },
                "moved",
            )
            .await
        }
        DownloadCommands::PauseAll => {
            simple_mutation(
                format,
                client,
                ApiRequest::PauseAll,
                "All downloads paused.",
            )
            .await
        }
        DownloadCommands::ResumeAll => {
            simple_mutation(
                format,
                client,
                ApiRequest::ResumeAll,
                "All downloads resumed.",
            )
            .await
        }
    }
}

fn add_options(args: &AddArgs) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for header in &args.headers {
        if !header.contains(':') {
            bail!("header must use NAME: VALUE form");
        }
    }
    if !args.headers.is_empty() {
        out.insert("header".into(), args.headers.join("\n"));
    }
    if let Some(v) = &args.referer {
        out.insert("referer".into(), v.clone());
    }
    if let Some(v) = &args.user_agent {
        out.insert("user-agent".into(), v.clone());
    }
    if let Some(v) = &args.checksum {
        out.insert("checksum".into(), v.clone());
    }
    if let Some(v) = args.connections {
        out.insert("max-connection-per-server".into(), v.to_string());
    }
    if let Some(v) = args.split {
        out.insert("split".into(), v.to_string());
    }
    if let Some(v) = &args.max_download_limit {
        out.insert(
            "max-download-limit".into(),
            units::format_limit(units::parse_limit(v)?),
        );
    }
    if args.paused {
        out.insert("pause".into(), "true".into());
    }
    for option in &args.aria2_options {
        let (key, value) = option
            .split_once('=')
            .ok_or_else(|| eyre!("aria2 option must use KEY=VALUE form"))?;
        if out.insert(key.into(), value.into()).is_some() {
            bail!("duplicate aria2 option '{key}'");
        }
    }
    Ok(out)
}

async fn queue(format: OutputFormat, client: &Client, command: QueueCommands) -> Result<()> {
    match command {
        QueueCommands::List => emit_items(
            format,
            "queue.list",
            client.snapshot().await?.current_downloads,
        ),
        QueueCommands::Move { selector, offset } => {
            mutate_selected(
                format,
                client,
                &selector,
                |gid| ApiRequest::ChangePosition { gid, offset },
                "moved",
            )
            .await
        }
        QueueCommands::Pause => {
            simple_mutation(format, client, ApiRequest::PauseAll, "Queue paused.").await
        }
        QueueCommands::Resume => {
            simple_mutation(format, client, ApiRequest::ResumeAll, "Queue resumed.").await
        }
    }
}

async fn history(format: OutputFormat, client: &Client, command: HistoryCommands) -> Result<()> {
    match command {
        HistoryCommands::List(args) => {
            let s = client.snapshot().await?;
            emit_items(
                format,
                "history.list",
                filter_items(&s.history_downloads, &args),
            )
        }
        HistoryCommands::Show(sel) => show_item(format, client, &sel, true).await,
        HistoryCommands::Remove(sel) => {
            mutate_selected(
                format,
                client,
                &sel,
                |gid| ApiRequest::RemoveHistory { gid },
                "removed from history",
            )
            .await
        }
        HistoryCommands::Purge { yes } => {
            if !yes {
                bail!("history purge requires --yes");
            }
            simple_mutation(format, client, ApiRequest::PurgeHistory, "History purged.").await
        }
    }
}

async fn speed(format: OutputFormat, client: &Client, command: SpeedCommands) -> Result<()> {
    match command {
        SpeedCommands::Show => {
            let s = client.snapshot().await?;
            emit(
                format,
                "speed.show",
                serde_json::to_value(&s.scheduler)?,
                format!(
                    "Mode: {:?}\nEffective limit: {}",
                    s.scheduler.mode,
                    fmt_limit(s.scheduler.effective_limit_bps)
                ),
            )
        }
        SpeedCommands::Set { limit } => {
            simple_mutation(
                format,
                client,
                ApiRequest::SetManualLimit {
                    limit_bps: units::parse_limit(&limit)?,
                },
                "Manual speed limit updated.",
            )
            .await
        }
        SpeedCommands::Unlimited => {
            simple_mutation(
                format,
                client,
                ApiRequest::SetManualLimit { limit_bps: None },
                "Manual speed limit is unlimited.",
            )
            .await
        }
        SpeedCommands::Mode { mode } => {
            simple_mutation(
                format,
                client,
                ApiRequest::SetMode {
                    mode: match mode {
                        ModeArg::Manual => ManualOrScheduled::Manual,
                        ModeArg::Scheduled => ManualOrScheduled::Scheduled,
                    },
                },
                "Speed mode updated.",
            )
            .await
        }
        SpeedCommands::Usual { limit } => {
            simple_mutation(
                format,
                client,
                ApiRequest::SetUsualInternetSpeed {
                    limit_bps: units::parse_limit(&limit)?,
                },
                "Usual internet speed updated.",
            )
            .await
        }
    }
}

async fn schedule(format: OutputFormat, client: &Client, command: ScheduleCommands) -> Result<()> {
    let mut limits = client
        .snapshot()
        .await?
        .scheduler
        .schedule_limits_bps
        .to_vec();
    match command {
        ScheduleCommands::Show => emit(
            format,
            "schedule.show",
            json!({"limits_bps":limits}),
            limits
                .iter()
                .enumerate()
                .map(|(h, v)| format!("{h:02}:00  {}", fmt_limit(*v)))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        ScheduleCommands::Set { limits: values } => {
            limits = values
                .iter()
                .map(|v| units::parse_limit(v))
                .collect::<Result<Vec<_>>>()?;
            simple_mutation(
                format,
                client,
                ApiRequest::SetSchedule { limits_bps: limits },
                "Schedule replaced.",
            )
            .await
        }
        ScheduleCommands::SetRange { from, to, limit } => {
            if from > 23 || to > 23 {
                bail!("hours must be between 0 and 23");
            }
            let value = units::parse_limit(&limit)?;
            let mut h = from;
            loop {
                if h == to {
                    break;
                }
                limits[h as usize] = value;
                h = (h + 1) % 24;
                if h == from {
                    break;
                }
            }
            simple_mutation(
                format,
                client,
                ApiRequest::SetSchedule { limits_bps: limits },
                "Schedule range updated.",
            )
            .await
        }
        ScheduleCommands::Clear => {
            simple_mutation(
                format,
                client,
                ApiRequest::SetSchedule {
                    limits_bps: vec![None; 24],
                },
                "Schedule cleared to unlimited.",
            )
            .await
        }
    }
}

async fn route(format: OutputFormat, client: &Client, command: RouteCommands) -> Result<()> {
    let s = client.snapshot().await?;
    let default = s.routing.default_download_dir.clone();
    let mut rules = s.routing.rules.clone();
    match command {
        RouteCommands::List => emit(
            format,
            "route.list",
            serde_json::to_value(&s.routing)?,
            format!(
                "Default: {default}\n{}",
                rules
                    .iter()
                    .enumerate()
                    .map(|(i, r)| format!("{i}: {} -> {}", r.pattern, r.directory))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        ),
        RouteCommands::Test { filename } => {
            let matched = match_rule(&default, &rules, &filename)?;
            emit(
                format,
                "route.test",
                json!({"filename":filename,"index":matched.index,"rule":matched.rule,"resolved_directory":matched.resolved_directory}),
                format!(
                    "Matched rule {} -> {}",
                    matched.index,
                    matched.resolved_directory.display()
                ),
            )
        }
        RouteCommands::SetDefault { directory } => {
            set_routes(
                format,
                client,
                expand_cli_path(&directory)?,
                rules,
                "Default route updated.",
            )
            .await
        }
        RouteCommands::Add {
            pattern,
            directory,
            before,
        } => {
            let rule = DownloadRoutingRule {
                pattern,
                directory: expand_cli_path(&directory)?,
            };
            let at = before.unwrap_or(rules.len()).min(rules.len());
            rules.insert(at, rule);
            set_routes(format, client, default, rules, "Route added.").await
        }
        RouteCommands::Update {
            index,
            pattern,
            directory,
        } => {
            let r = rules
                .get_mut(index)
                .ok_or_else(|| eyre!("route index {index} not found"))?;
            if let Some(v) = pattern {
                r.pattern = v
            }
            if let Some(v) = directory {
                r.directory = expand_cli_path(&v)?
            }
            set_routes(format, client, default, rules, "Route updated.").await
        }
        RouteCommands::Remove { index } => {
            if index >= rules.len() {
                bail!("route index {index} not found")
            }
            rules.remove(index);
            set_routes(format, client, default, rules, "Route removed.").await
        }
        RouteCommands::Move { index, offset } => {
            if index >= rules.len() {
                bail!("route index {index} not found")
            }
            let target = (index as i32 + offset).clamp(0, rules.len() as i32 - 1) as usize;
            let item = rules.remove(index);
            rules.insert(target, item);
            set_routes(format, client, default, rules, "Route moved.").await
        }
    }
}

async fn set_routes(
    format: OutputFormat,
    client: &Client,
    default: String,
    rules: Vec<DownloadRoutingRule>,
    message: &str,
) -> Result<()> {
    simple_mutation(
        format,
        client,
        ApiRequest::SetDownloadRouting {
            default_download_dir: default,
            rules,
        },
        message,
    )
    .await
}

async fn torrent(format: OutputFormat, client: &Client, command: TorrentCommands) -> Result<()> {
    let s = client.snapshot().await?;
    match command {
        TorrentCommands::Streaming {
            command: StreamingCommands::Show,
        } => emit(
            format,
            "torrent.streaming.show",
            serde_json::to_value(&s.torrents)?,
            format!(
                "Mode: {:?}\nHead: {} MiB\nTail: {} MiB",
                s.torrents.mode, s.torrents.head_size_mib, s.torrents.tail_size_mib
            ),
        ),
        TorrentCommands::Streaming {
            command:
                StreamingCommands::Set {
                    mode,
                    head_mib,
                    tail_mib,
                },
        } => {
            simple_mutation(
                format,
                client,
                ApiRequest::SetTorrentStreamingSettings {
                    mode: match mode {
                        StreamingModeArg::Off => TorrentStreamingMode::Off,
                        StreamingModeArg::StartFirst => TorrentStreamingMode::StartFirst,
                        StreamingModeArg::StartAndEndFirst => {
                            TorrentStreamingMode::StartAndEndFirst
                        }
                    },
                    head_size_mib: head_mib,
                    tail_size_mib: tail_mib,
                },
                "Torrent streaming settings updated.",
            )
            .await
        }
        TorrentCommands::Show(sel) | TorrentCommands::Files(sel) | TorrentCommands::Peers(sel) => {
            let item = select(&s.current_downloads, &s.history_downloads, &sel)?;
            let detail = s
                .torrents
                .downloads
                .iter()
                .find(|v| v.gid == item.gid)
                .ok_or_else(|| eyre!("{} is not a torrent", item.gid))?;
            emit(
                format,
                "torrent.show",
                serde_json::to_value(detail)?,
                format!(
                    "{} ({})\nFiles: {}\nPeers: {} live, {} seen",
                    detail.name,
                    detail.gid,
                    detail.files.len(),
                    detail.live_peers,
                    detail.seen_peers
                ),
            )
        }
    }
}

async fn web(format: OutputFormat, client: &Client, command: WebCommands) -> Result<()> {
    let s = client.snapshot().await?;
    let w = s.web_ui;
    match command {
        WebCommands::Status => emit(
            format,
            "web.status",
            serde_json::to_value(&w)?,
            format!(
                "Web UI: {:?}\nURL: {}\nPending PINs: {}",
                w.status,
                w.url,
                w.pending_pair_pins.len()
            ),
        ),
        WebCommands::Enable => {
            set_web(
                format,
                client,
                true,
                w.bind_address,
                w.port,
                w.cookie_days,
                "Web UI enabled.",
            )
            .await
        }
        WebCommands::Disable => {
            set_web(
                format,
                client,
                false,
                w.bind_address,
                w.port,
                w.cookie_days,
                "Web UI disabled.",
            )
            .await
        }
        WebCommands::Configure {
            bind,
            port,
            cookie_days,
        } => {
            set_web(
                format,
                client,
                w.enabled,
                bind.unwrap_or(w.bind_address),
                port.unwrap_or(w.port),
                cookie_days.unwrap_or(w.cookie_days),
                "Web UI configuration updated.",
            )
            .await
        }
        WebCommands::Pairing {
            command: PairingCommands::List,
        } => emit(
            format,
            "web.pairing.list",
            json!({"pins":w.pending_pair_pins}),
            format!(
                "Pending PINs: {}",
                if w.pending_pair_pins.is_empty() {
                    "none".into()
                } else {
                    w.pending_pair_pins.join(", ")
                }
            ),
        ),
        WebCommands::Pairing {
            command: PairingCommands::Approve { pin },
        } => {
            simple_mutation(
                format,
                client,
                ApiRequest::ApproveWebUiPin { pin },
                "Pairing approved.",
            )
            .await
        }
        WebCommands::Session {
            command: SessionCommands::List,
        } => emit(
            format,
            "web.session.list",
            json!({"active_session_count":w.active_session_count}),
            format!("Active Web UI sessions: {}", w.active_session_count),
        ),
        WebCommands::Session {
            command: SessionCommands::RevokeAll { yes },
        } => {
            if !yes {
                bail!("revoking all Web UI sessions requires --yes")
            }
            simple_mutation(
                format,
                client,
                ApiRequest::RevokeAllWebUiSessions,
                "Revoked all Web UI sessions.",
            )
            .await
        }
    }
}
async fn set_web(
    format: OutputFormat,
    client: &Client,
    enabled: bool,
    bind: String,
    port: u16,
    cookie_days: u32,
    msg: &str,
) -> Result<()> {
    simple_mutation(
        format,
        client,
        ApiRequest::SetWebUiSettings {
            enabled,
            bind_address: bind,
            port,
            cookie_days,
        },
        msg,
    )
    .await
}

async fn webhook(format: OutputFormat, client: &Client, command: WebhookCommands) -> Result<()> {
    let w = client.snapshot().await?.webhooks;
    match command {
        WebhookCommands::Show => emit(
            format,
            "webhook.show",
            json!({"enabled":w.enabled,"ping_mode":w.ping_mode,"ping_id":w.ping_id,"url_configured":!w.discord_webhook_url.is_empty()}),
            format!(
                "Webhook: {}\nPing: {:?}",
                if w.enabled { "enabled" } else { "disabled" },
                w.ping_mode
            ),
        ),
        WebhookCommands::Configure {
            url,
            ping_mode,
            ping_id,
        } => {
            simple_mutation(
                format,
                client,
                ApiRequest::SetWebhookSettings {
                    discord_webhook_url: url,
                    ping_mode: match ping_mode {
                        PingModeArg::None => WebhookPingMode::None,
                        PingModeArg::Everyone => WebhookPingMode::Everyone,
                        PingModeArg::SpecificId => WebhookPingMode::SpecificId,
                    },
                    ping_id,
                },
                "Webhook configured.",
            )
            .await
        }
        WebhookCommands::Disable => {
            simple_mutation(
                format,
                client,
                ApiRequest::SetWebhookSettings {
                    discord_webhook_url: String::new(),
                    ping_mode: WebhookPingMode::None,
                    ping_id: None,
                },
                "Webhook disabled.",
            )
            .await
        }
        WebhookCommands::Test => {
            simple_mutation(
                format,
                client,
                ApiRequest::TriggerWebhookTest,
                "Test webhook sent.",
            )
            .await
        }
    }
}

async fn config(format: OutputFormat, client: &Client, command: ConfigCommands) -> Result<()> {
    let s = client.snapshot().await?;
    match command {
        ConfigCommands::Show => emit(
            format,
            "config.show",
            json!({"daemon":s.daemon_status,"scheduler":s.scheduler,"torrents":s.torrents,"routing":s.routing,"webhooks":{"enabled":s.webhooks.enabled,"ping_mode":s.webhooks.ping_mode,"ping_id":s.webhooks.ping_id},"web_ui":s.web_ui}),
            "Configuration loaded from the daemon. Use --json for the complete redacted view."
                .into(),
        ),
        ConfigCommands::Validate => emit(
            format,
            "config.validate",
            json!({"valid":true,"warnings":s.warnings}),
            if s.warnings.is_empty() {
                "Configuration is valid.".into()
            } else {
                format!(
                    "Configuration is valid with warnings: {}",
                    s.warnings.join("; ")
                )
            },
        ),
    }
}

async fn mutate_selected<F>(
    format: OutputFormat,
    client: &Client,
    selector: &Selector,
    make: F,
    verb: &str,
) -> Result<()>
where
    F: FnOnce(String) -> ApiRequest,
{
    let s = client.snapshot().await?;
    let item = select(&s.current_downloads, &s.history_downloads, selector)?;
    let gid = item.gid.clone();
    let name = item.name.clone();
    let response = client.mutate(make(gid.clone())).await?;
    let download = response.result.and_then(|snapshot| {
        snapshot
            .current_downloads
            .into_iter()
            .chain(snapshot.history_downloads)
            .find(|item| item.gid == gid)
    });
    emit(
        format,
        "download.mutate",
        json!({"gid":gid,"name":name,"action":verb,"download":download}),
        format!("{} ({}) {verb}.", name, gid),
    )
}
async fn simple_mutation(
    format: OutputFormat,
    client: &Client,
    request: ApiRequest,
    human: &str,
) -> Result<()> {
    client.mutate(request).await?;
    emit(format, "mutation", json!({"applied":true}), human.into())
}
async fn show_item(
    format: OutputFormat,
    client: &Client,
    selector: &Selector,
    history_only: bool,
) -> Result<()> {
    let s = client.snapshot().await?;
    let item = if history_only {
        select(&[], &s.history_downloads, selector)?
    } else {
        select(&s.current_downloads, &s.history_downloads, selector)?
    };
    emit(
        format,
        "download.show",
        serde_json::to_value(item)?,
        human_item(item),
    )
}
async fn files(format: OutputFormat, client: &Client, selector: &Selector) -> Result<()> {
    let s = client.snapshot().await?;
    let item = select(&s.current_downloads, &s.history_downloads, selector)?;
    if let Some(t) = s.torrents.downloads.iter().find(|t| t.gid == item.gid) {
        emit(
            format,
            "download.files",
            serde_json::to_value(&t.files)?,
            t.files
                .iter()
                .map(|f| {
                    format!(
                        "{}  {}  {}",
                        if f.included { "included" } else { "excluded" },
                        units::format_bytes(f.length),
                        f.name
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        emit(
            format,
            "download.files",
            json!([{"path":item.primary_path,"total_bytes":item.total_bytes}]),
            item.primary_path
                .clone()
                .unwrap_or_else(|| "No file path reported yet.".into()),
        )
    }
}

async fn wait_download(
    format: OutputFormat,
    client: &Client,
    selector: Selector,
    until: &str,
    max: Duration,
    interval: Duration,
) -> Result<()> {
    let wanted = until
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    let start = Instant::now();
    loop {
        let s = client.snapshot().await?;
        let item = select(&s.current_downloads, &s.history_downloads, &selector)?;
        let status = format!("{:?}", item.status).to_ascii_lowercase();
        if wanted.contains(&status) {
            return emit(
                format,
                "download.wait",
                serde_json::to_value(item)?,
                human_item(item),
            );
        }
        if start.elapsed() >= max {
            bail!("timed out waiting for download; last status was {status}")
        }
        tokio::time::sleep(interval).await
    }
}

fn select<'a>(
    current: &'a [DownloadItem],
    history: &'a [DownloadItem],
    selector: &Selector,
) -> Result<&'a DownloadItem> {
    let matches = current
        .iter()
        .chain(history)
        .filter(|i| {
            selector.gid.as_ref().is_some_and(|g| &i.gid == g)
                || selector.name.as_ref().is_some_and(|n| &i.name == n)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [item] => Ok(*item),
        [] => bail!("download not found"),
        items => bail!(
            "selector is ambiguous; matched {} downloads: {}",
            items.len(),
            items
                .iter()
                .map(|i| format!("{} ({})", i.name, i.gid))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
fn filter_items(items: &[DownloadItem], args: &ListArgs) -> Vec<DownloadItem> {
    items
        .iter()
        .filter(|i| {
            args.status.is_empty()
                || args
                    .status
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(&format!("{:?}", i.status)))
        })
        .filter(|i| {
            args.name.as_ref().is_none_or(|n| {
                i.name
                    .to_ascii_lowercase()
                    .contains(&n.to_ascii_lowercase())
            })
        })
        .cloned()
        .collect()
}

fn emit_items(format: OutputFormat, command: &str, items: Vec<DownloadItem>) -> Result<()> {
    let human = if items.is_empty() {
        "No downloads matched.".into()
    } else {
        items.iter().map(human_item).collect::<Vec<_>>().join("\n")
    };
    emit(
        format,
        command,
        json!({"items":items,"count":items.len()}),
        human,
    )
}
fn emit(format: OutputFormat, command: &str, data: Value, human: String) -> Result<()> {
    match format {
        OutputFormat::Human => println!("{human}"),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"api_version":"1","ok":true,"command":command,"data":data})
            )?
        ),
        OutputFormat::Jsonl => println!(
            "{}",
            serde_json::to_string(
                &json!({"api_version":"1","ok":true,"command":command,"data":data})
            )?
        ),
        OutputFormat::Tsv => emit_tsv(&data),
    }
    Ok(())
}
fn emit_tsv(data: &Value) {
    match data {
        Value::Array(values) => {
            for v in values {
                println!("{}", scalar_row(v))
            }
        }
        Value::Object(map) if map.get("items").and_then(Value::as_array).is_some() => {
            for v in map["items"].as_array().unwrap() {
                println!("{}", scalar_row(v))
            }
        }
        _ => println!("{}", scalar_row(data)),
    }
}
fn scalar_row(v: &Value) -> String {
    match v {
        Value::Object(m) => m
            .values()
            .filter(|v| !v.is_object() && !v.is_array())
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            })
            .collect::<Vec<_>>()
            .join("\t"),
        _ => v.to_string(),
    }
}
fn human_item(i: &DownloadItem) -> String {
    let pct = if i.total_bytes == 0 {
        0.0
    } else {
        i.completed_bytes as f64 / i.total_bytes as f64 * 100.0
    };
    format!(
        "{}  {:?}  {:.1}%  {}  {}",
        i.gid,
        i.status,
        pct,
        units::format_bytes_per_sec(i.download_speed_bps),
        i.name
    )
}
fn human_status(s: &Snapshot) -> String {
    format!(
        "AriaTUI daemon is {:?}\n  Socket:       {}\n  aria2c:       {:?}{}\n  Downloads:    {} active, {} waiting, {} completed\n  Speed:        {}\n  Limit:        {} ({:?})\n  Warnings:     {}",
        s.aria2_status.lifecycle,
        s.daemon_status.socket_path,
        s.aria2_status.lifecycle,
        s.aria2_status
            .pid
            .map(|p| format!(" (pid {p})"))
            .unwrap_or_default(),
        s.global.num_active,
        s.global.num_waiting,
        s.global.num_stopped,
        units::format_bytes_per_sec(s.global.download_speed_bps),
        fmt_limit(s.scheduler.effective_limit_bps),
        s.scheduler.mode,
        if s.warnings.is_empty() {
            "none".into()
        } else {
            s.warnings.join("; ")
        }
    )
}
fn status_value(s: &Snapshot) -> Value {
    json!({"daemon":s.daemon_status,"aria2":s.aria2_status,"global":s.global,"scheduler":s.scheduler,"warnings":s.warnings})
}
fn fmt_limit(v: Option<u64>) -> String {
    v.map(units::format_bytes_per_sec)
        .unwrap_or_else(|| "unlimited".into())
}
fn expand_cli_path(input: &str) -> Result<String> {
    let path = if let Some(rest) = input.strip_prefix("~/") {
        let home = std::env::var_os("HOME").ok_or_else(|| eyre!("HOME is not set"))?;
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(input)
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(absolute.display().to_string())
}
fn parse_duration(input: &str) -> Result<Duration> {
    let v = input.trim();
    let at = v
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(v.len());
    let n: f64 = v[..at]
        .parse()
        .map_err(|_| eyre!("invalid duration '{input}'"))?;
    let factor = match v[at..].trim() {
        "ms" => 0.001,
        "" | "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        unit => bail!("unsupported duration unit '{unit}'"),
    };
    if !n.is_finite() || n < 0.0 {
        bail!("invalid duration '{input}'")
    }
    Ok(Duration::from_secs_f64(n * factor))
}
fn capabilities() -> Value {
    json!({"api_version":"1","output_formats":["human","json","jsonl","tsv"],"commands":["status","doctor","capabilities","schema","events","batch","api","download","queue","history","speed","schedule","route","torrent","web","webhook","config","service"],"features":{"explicit_destination":true,"idempotent_add":true,"event_stream":true,"batch":true,"raw_typed_api":true,"unix_peer_credentials":true,"root_daemon_path_policy":true},"download_add":{"schemes":["http","https","ftp","sftp","magnet"],"curated_options":["header","referer","user-agent","checksum","connections","split","max-download-limit","paused"],"escape_hatch":"--aria2-option KEY=VALUE"}})
}
fn schema(command: Option<&str>) -> Value {
    let input = match command {
        Some("download.add") => json!({
            "type":"object",
            "required":["url"],
            "properties":{
                "url":{"type":"string"},
                "dir":{"type":["string","null"]},
                "output_name":{"type":["string","null"]},
                "headers":{"type":"array","items":{"type":"string"}},
                "referer":{"type":["string","null"]},
                "user_agent":{"type":["string","null"]},
                "checksum":{"type":["string","null"]},
                "connections":{"type":["integer","null"]},
                "split":{"type":["integer","null"]},
                "max_download_limit":{"type":["string","null"]},
                "aria2_options":{"type":"object","additionalProperties":{"type":"string"}},
                "paused":{"type":"boolean"},
                "idempotency_key":{"type":["string","null"]}
            }
        }),
        Some("batch") => json!({"type":"array","items":{"$ref":"api-request"}}),
        Some("api-request") => {
            json!({"type":"object","required":["method"],"properties":{"method":{"type":"string"},"params":{"type":"object"}}})
        }
        _ => Value::Null,
    };
    json!({"$schema":"https://json-schema.org/draft/2020-12/schema","command":command,"input":input,"response":{"type":"object","required":["api_version","ok"],"properties":{"api_version":{"const":"1"},"ok":{"type":"boolean"},"command":{"type":"string"},"data":{},"error":{"type":"object"}}},"discovery":"Run `<command> --help` for CLI flags. Known detailed schemas: download.add, batch, api-request."})
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    #[test]
    fn durations() {
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert!(parse_duration("tomorrow").is_err());
    }
    #[test]
    fn option_parser() {
        let args = AddArgs {
            url: "https://x/y".into(),
            dir: None,
            output_name: None,
            headers: vec!["Accept: x".into()],
            referer: None,
            user_agent: None,
            checksum: None,
            connections: Some(4),
            split: None,
            max_download_limit: None,
            aria2_options: vec!["continue=true".into()],
            paused: false,
            dry_run: false,
            idempotency_key: None,
        };
        let o = add_options(&args).unwrap();
        assert_eq!(o["max-connection-per-server"], "4");
        assert_eq!(o["continue"], "true");
    }
    #[test]
    fn skill_documents_real_top_level_commands() {
        let skill = include_str!("../ariatui-skill.md");
        let command = Cli::command();
        for name in [
            "status",
            "doctor",
            "capabilities",
            "schema",
            "events",
            "batch",
            "api",
            "download",
            "queue",
            "history",
            "speed",
            "schedule",
            "route",
            "torrent",
            "web",
            "webhook",
            "config",
            "service",
        ] {
            assert!(
                command
                    .get_subcommands()
                    .any(|item| item.get_name() == name)
            );
            assert!(skill.contains(&format!("ariatui {name}")));
        }
    }
}
