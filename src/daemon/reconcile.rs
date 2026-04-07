use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Local;
use color_eyre::eyre::{Result, eyre};
use reqwest::{
    StatusCode,
    header::{CONTENT_DISPOSITION, RANGE},
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tracing::{error, warn};

use crate::{
    daemon::{
        AppContext, child,
        snapshot::{
            ApiPayload, ApiReply, Aria2ChildStatus, ChildLifecycle, DownloadItem, DownloadStatus,
            GlobalStats, ResolvedHttpUrl, RoutingSnapshot, SchedulerSnapshot, Snapshot,
            WebUiStatus, WebhookSnapshot,
        },
    },
    routing::{match_rule, normalize_rules},
    rpc::{
        client::Aria2RpcClient,
        types::{Aria2File, Aria2GlobalStat, Aria2Status},
    },
    schedule, units, web,
    webhook::{
        WebhookPingMode, mention_prefix, validate_discord_webhook_url, validate_ping_id,
        webhook_enabled,
    },
};

#[derive(Debug)]
pub struct RuntimeAria2 {
    pub rpc: Aria2RpcClient,
    pub child: tokio::process::Child,
}

#[derive(Debug)]
pub struct DaemonState {
    pub app: Arc<AppContext>,
    pub runtime: Mutex<Option<RuntimeAria2>>,
    pub snapshot: RwLock<Snapshot>,
    pub desired_limit_bps: RwLock<Option<u64>>,
    pub log_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub seen_terminal_events: Mutex<HashSet<String>>,
    pub notifications_initialized: Mutex<bool>,
    pub last_notified_restart_count: Mutex<u32>,
    pub web_pairings: Mutex<HashMap<String, WebPairing>>,
    pub web_sessions: Mutex<HashMap<String, Instant>>,
}

#[derive(Debug, Clone)]
pub struct WebPairing {
    pub pin: String,
    pub expires_at: Instant,
    pub approved_session_token: Option<String>,
}

pub type SharedDaemonState = Arc<DaemonState>;

impl DaemonState {
    pub async fn new(app: Arc<AppContext>) -> Result<Self> {
        app.paths.ensure_dirs()?;
        if !app.paths.aria2_session_file.exists() {
            tokio::fs::write(&app.paths.aria2_session_file, "").await?;
        }
        let snapshot = Snapshot::empty(
            app.config
                .daemon
                .socket_path
                .clone()
                .if_empty_then(app.paths.socket_path.display().to_string()),
            app.paths.state_file.display().to_string(),
            app.paths.config_file.display().to_string(),
            app.current_executable_path.clone(),
            app.current_build_id.clone(),
        );
        Ok(Self {
            app,
            runtime: Mutex::new(None),
            snapshot: RwLock::new(snapshot),
            desired_limit_bps: RwLock::new(None),
            log_task: Mutex::new(None),
            seen_terminal_events: Mutex::new(HashSet::new()),
            notifications_initialized: Mutex::new(false),
            last_notified_restart_count: Mutex::new(0),
            web_pairings: Mutex::new(HashMap::new()),
            web_sessions: Mutex::new(HashMap::new()),
        })
    }

    pub async fn ensure_runtime(&self) -> Result<()> {
        let mut runtime = self.runtime.lock().await;
        if runtime.is_some() {
            return Ok(());
        }
        self.spawn_runtime(&mut runtime, ChildLifecycle::Starting)
            .await
    }

    async fn spawn_runtime(
        &self,
        runtime_slot: &mut Option<RuntimeAria2>,
        lifecycle: ChildLifecycle,
    ) -> Result<()> {
        self.set_lifecycle(lifecycle).await;
        let (child_process, rx) =
            child::spawn_aria2(&self.app.config, self.app.paths.aria2_session_file.clone()).await?;

        let endpoint = format!("http://127.0.0.1:{}/jsonrpc", child_process.port);
        let rpc = Aria2RpcClient::new(
            endpoint,
            child_process.secret.clone(),
            Duration::from_secs(self.app.config.daemon.rpc_request_timeout_secs),
        )?;
        child::wait_for_rpc_ready(Duration::from_secs(10), || {
            let rpc = rpc.clone();
            async move {
                let _: Value = rpc.call("aria2.getVersion", vec![]).await?;
                Ok(())
            }
        })
        .await?;
        let handle = tokio::spawn(child::log_child_output(rx));
        *self.log_task.lock().await = Some(handle);
        let pid = child_process.process.id();
        *runtime_slot = Some(RuntimeAria2 {
            rpc,
            child: child_process.process,
        });
        {
            let mut snapshot = self.snapshot.write().await;
            snapshot.aria2_status = Aria2ChildStatus {
                lifecycle: ChildLifecycle::Ready,
                pid,
                rpc_port: Some(child_process.port),
                restart_count: snapshot.aria2_status.restart_count,
                last_exit: snapshot.aria2_status.last_exit.clone(),
                last_error: None,
            };
        }
        Ok(())
    }

    async fn set_lifecycle(&self, lifecycle: ChildLifecycle) {
        let mut snapshot = self.snapshot.write().await;
        snapshot.aria2_status.lifecycle = lifecycle;
    }

    pub async fn snapshot(&self) -> Snapshot {
        self.snapshot.read().await.clone()
    }

    pub async fn perform_refresh(&self) -> Result<()> {
        self.ensure_runtime().await?;
        self.check_child_exit().await?;
        let runtime = self.runtime.lock().await;
        let runtime = runtime
            .as_ref()
            .ok_or_else(|| eyre!("aria2 runtime missing"))?;

        let active: Vec<Aria2Status> = runtime
            .rpc
            .call(
                "aria2.tellActive",
                vec![json!([
                    "gid",
                    "status",
                    "totalLength",
                    "completedLength",
                    "downloadSpeed",
                    "uploadSpeed",
                    "connections",
                    "errorCode",
                    "errorMessage",
                    "files"
                ])],
            )
            .await?;
        let waiting: Vec<Aria2Status> = runtime
            .rpc
            .call(
                "aria2.tellWaiting",
                vec![
                    json!(0),
                    json!(self.app.config.daemon.waiting_limit),
                    json!([
                        "gid",
                        "status",
                        "totalLength",
                        "completedLength",
                        "downloadSpeed",
                        "uploadSpeed",
                        "connections",
                        "errorCode",
                        "errorMessage",
                        "files"
                    ]),
                ],
            )
            .await?;
        let stopped: Vec<Aria2Status> = runtime
            .rpc
            .call(
                "aria2.tellStopped",
                vec![
                    json!(0),
                    json!(self.app.config.daemon.stopped_history_limit),
                    json!([
                        "gid",
                        "status",
                        "totalLength",
                        "completedLength",
                        "downloadSpeed",
                        "uploadSpeed",
                        "connections",
                        "errorCode",
                        "errorMessage",
                        "files"
                    ]),
                ],
            )
            .await?;
        let global: Aria2GlobalStat = runtime.rpc.call("aria2.getGlobalStat", vec![]).await?;

        let state = self.app.state.read().await.clone();
        let resolved = schedule::resolve(Local::now(), &state)?;
        let mut desired_limit = self.desired_limit_bps.write().await;
        if *desired_limit != resolved.effective_limit_bps {
            self.apply_speed_limit(runtime, resolved.effective_limit_bps)
                .await?;
            *desired_limit = resolved.effective_limit_bps;
        }

        let mut snapshot = self.snapshot.write().await;
        snapshot.scheduler = SchedulerSnapshot {
            mode: state.mode,
            manual_limit_bps: state.manual_limit_bps()?,
            usual_internet_speed_bps: state.usual_internet_speed_bps()?,
            schedule_limits_bps: resolved.schedule_limits_bps,
            effective_limit_bps: resolved.effective_limit_bps,
            current_hour: resolved.current_hour,
            next_change_at_local: resolved.next_change_at_local,
            remembered_cancel_behavior: state.remembered_cancel_behavior,
        };
        snapshot.routing = RoutingSnapshot {
            default_download_dir: state.default_download_dir.clone(),
            rules: normalize_rules(&state.default_download_dir, &state.download_rules),
        };
        snapshot.webhooks = WebhookSnapshot {
            discord_webhook_url: state.discord_webhook_url.clone(),
            enabled: webhook_enabled(&state.discord_webhook_url),
            ping_mode: state.webhook_ping_mode,
            ping_id: validate_ping_id(state.webhook_ping_mode, Some(&state.webhook_ping_id))?,
        };
        if snapshot.web_ui.url.is_empty() {
            snapshot.web_ui.url =
                web::format_listener_url(&state.web_ui_bind_address, state.web_ui_port);
        }
        let (pending_pair_pins, active_session_count) = web::auth_summary(self).await;
        snapshot.web_ui.enabled = state.web_ui_enabled;
        snapshot.web_ui.bind_address = state.web_ui_bind_address.clone();
        snapshot.web_ui.port = state.web_ui_port;
        snapshot.web_ui.cookie_days = state.web_ui_cookie_days;
        snapshot.web_ui.auth_configured = true;
        snapshot.web_ui.pending_pair_pins = pending_pair_pins;
        snapshot.web_ui.active_session_count = active_session_count;
        snapshot.global = parse_global(global);
        snapshot.current_downloads = active
            .into_iter()
            .chain(waiting.into_iter())
            .map(map_status)
            .collect();
        snapshot.history_downloads = stopped.into_iter().map(map_status).collect();
        let snapshot_copy = snapshot.clone();
        drop(snapshot);
        self.process_webhook_events(&snapshot_copy).await;
        self.write_snapshot_cache(&snapshot_copy).await;

        Ok(())
    }

    async fn write_snapshot_cache(&self, snapshot: &Snapshot) {
        match serde_json::to_vec(snapshot) {
            Ok(encoded) => {
                if let Err(error) =
                    tokio::fs::write(&self.app.paths.snapshot_cache_file, encoded).await
                {
                    warn!(
                        "failed to write snapshot cache {}: {error}",
                        self.app.paths.snapshot_cache_file.display()
                    );
                }
            }
            Err(error) => {
                warn!("failed to encode snapshot cache: {error}");
            }
        }
    }

    async fn apply_speed_limit(
        &self,
        runtime: &RuntimeAria2,
        limit_bps: Option<u64>,
    ) -> Result<()> {
        let value = limit_bps
            .map(|bps| bps.to_string())
            .unwrap_or_else(|| "0".into());
        let _: String = runtime
            .rpc
            .call(
                "aria2.changeGlobalOption",
                vec![json!({ "max-overall-download-limit": value })],
            )
            .await?;
        Ok(())
    }

    pub async fn execute(&self, request: crate::daemon::ApiRequest) -> Result<ApiReply> {
        let mut payload = None;
        match request {
            crate::daemon::ApiRequest::Ping | crate::daemon::ApiRequest::GetSnapshot => {}
            crate::daemon::ApiRequest::ResolveHttpUrl { url } => {
                payload = Some(ApiPayload::ResolvedHttpUrl(
                    self.resolve_http_url(&url).await?,
                ));
            }
            crate::daemon::ApiRequest::AddHttpUrl { url, filename } => {
                let state = self.app.state.read().await.clone();
                let filename = validate_download_filename(
                    filename.unwrap_or_else(|| filename_from_url(&url)).trim(),
                )?;
                let route = match_rule(
                    &state.default_download_dir,
                    &state.download_rules,
                    &filename,
                )?;
                tokio::fs::create_dir_all(&route.resolved_directory).await?;
                let _: String = self
                    .call(
                        "aria2.addUri",
                        vec![
                            json!([url]),
                            json!({
                                "dir": route.resolved_directory.display().to_string(),
                                "out": filename,
                            }),
                        ],
                    )
                    .await?;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::Pause { gid, force } => {
                let method = if force {
                    "aria2.forcePause"
                } else {
                    "aria2.pause"
                };
                let _: String = self.call(method, vec![json!(gid)]).await?;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::Resume { gid } => {
                let _: String = self.call("aria2.unpause", vec![json!(gid)]).await?;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::Cancel { gid, delete_files } => {
                self.cancel_download(&gid, delete_files).await?;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::RemoveHistory { gid } => {
                let _: String = self
                    .call("aria2.removeDownloadResult", vec![json!(gid)])
                    .await?;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::SetMode { mode } => {
                let mut state = self.app.state.write().await;
                state.mode = mode;
                state.save(&self.app.paths.state_file)?;
                drop(state);
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::SetManualLimit { limit_bps } => {
                let mut state = self.app.state.write().await;
                state.manual_limit = units::format_limit(limit_bps);
                state.save(&self.app.paths.state_file)?;
                drop(state);
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::SetUsualInternetSpeed { limit_bps } => {
                let mut state = self.app.state.write().await;
                state.usual_internet_speed = units::format_limit(limit_bps);
                state.save(&self.app.paths.state_file)?;
                drop(state);
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::SetSchedule { limits_bps } => {
                if limits_bps.len() != 24 {
                    return Err(eyre!("schedule must contain 24 entries"));
                }
                let mut state = self.app.state.write().await;
                state.schedule = limits_bps.into_iter().map(units::format_limit).collect();
                state.save(&self.app.paths.state_file)?;
                drop(state);
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::SetDownloadRouting {
                default_download_dir,
                rules,
            } => {
                let mut state = self.app.state.write().await;
                state.default_download_dir = default_download_dir;
                state.download_rules = rules;
                state.save(&self.app.paths.state_file)?;
                drop(state);
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::SetWebhookSettings {
                discord_webhook_url,
                ping_mode,
                ping_id,
            } => {
                validate_discord_webhook_url(&discord_webhook_url)?;
                let validated_ping_id = validate_ping_id(ping_mode, ping_id.as_deref())?;
                let mut state = self.app.state.write().await;
                state.discord_webhook_url = discord_webhook_url;
                state.webhook_ping_mode = ping_mode;
                state.webhook_ping_id = validated_ping_id.unwrap_or_default();
                state.save(&self.app.paths.state_file)?;
                drop(state);
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::TriggerWebhookTest => {
                self.send_test_webhook().await?;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::SetWebUiSettings {
                enabled,
                bind_address,
                port,
                cookie_days,
            } => {
                web::validate_bind_address(&bind_address)?;
                web::validate_cookie_days(cookie_days)?;
                if port == 0 {
                    return Err(eyre!("web ui port must be between 1 and 65535"));
                }
                let mut state = self.app.state.write().await;
                state.web_ui_enabled = enabled;
                state.web_ui_bind_address = bind_address;
                state.web_ui_port = port;
                state.web_ui_cookie_days = cookie_days;
                state.save(&self.app.paths.state_file)?;
                drop(state);
                web::set_web_snapshot(self, WebUiStatus::Starting, None, None).await;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::ApproveWebUiPin { pin } => {
                web::approve_pairing_pin(self, &pin).await?;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::SetRememberedCancelBehavior { behavior } => {
                let mut state = self.app.state.write().await;
                state.remembered_cancel_behavior = behavior;
                state.save(&self.app.paths.state_file)?;
                drop(state);
                self.perform_refresh().await?;
            }
        }
        Ok(ApiReply {
            snapshot: self.snapshot().await,
            payload,
        })
    }

    async fn process_webhook_events(&self, snapshot: &Snapshot) {
        let settings = snapshot.webhooks.clone();
        if !settings.enabled {
            return;
        }

        let mut initialized = self.notifications_initialized.lock().await;
        let mut seen = self.seen_terminal_events.lock().await;
        if !*initialized {
            for item in &snapshot.history_downloads {
                seen.insert(event_key(item));
            }
            *self.last_notified_restart_count.lock().await = snapshot.aria2_status.restart_count;
            *initialized = true;
            return;
        }

        let new_events = snapshot
            .history_downloads
            .iter()
            .filter(|item| is_notable_terminal_event(item))
            .filter(|item| seen.insert(event_key(item)))
            .cloned()
            .collect::<Vec<_>>();
        drop(seen);
        drop(initialized);

        for item in new_events {
            self.spawn_webhook_message(
                settings.clone(),
                webhook_title_for_item(&item),
                webhook_body_for_item(&item),
            );
        }

        let mut last_restart = self.last_notified_restart_count.lock().await;
        if snapshot.aria2_status.restart_count > *last_restart {
            *last_restart = snapshot.aria2_status.restart_count;
            self.spawn_webhook_message(
                settings,
                "AriaTUI: aria2 restarted".into(),
                format!(
                    "The managed aria2c process restarted.\nRestart count: {}\nLast exit: {}\nLast error: {}",
                    snapshot.aria2_status.restart_count,
                    snapshot
                        .aria2_status
                        .last_exit
                        .clone()
                        .unwrap_or_else(|| "-".into()),
                    snapshot
                        .aria2_status
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "-".into())
                ),
            );
        }
    }

    async fn send_test_webhook(&self) -> Result<()> {
        let state = self.app.state.read().await.clone();
        validate_discord_webhook_url(&state.discord_webhook_url)?;
        let ping_id = validate_ping_id(state.webhook_ping_mode, Some(&state.webhook_ping_id))?;
        let settings = WebhookSnapshot {
            discord_webhook_url: state.discord_webhook_url,
            enabled: true,
            ping_mode: state.webhook_ping_mode,
            ping_id,
        };
        post_discord_webhook(
            settings,
            "AriaTUI test notification".into(),
            "Dummy event: a test download finished successfully.\nName: example-release.iso\nSize: 1.4 GiB\nPath: ~/Downloads/example-release.iso\nSource: https://example.com/example-release.iso".into(),
        )
        .await?;
        Ok(())
    }

    fn spawn_webhook_message(&self, settings: WebhookSnapshot, title: String, description: String) {
        if !settings.enabled {
            return;
        }
        tokio::spawn(async move {
            if let Err(error) = post_discord_webhook(settings, title, description).await {
                warn!("failed to send webhook notification: {error}");
            }
        });
    }

    async fn resolve_http_url(&self, url: &str) -> Result<ResolvedHttpUrl> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(Duration::from_secs(
                self.app.config.daemon.rpc_request_timeout_secs.max(2),
            ))
            .build()?;
        let response = match client.head(url).send().await {
            Ok(response)
                if response.status() == StatusCode::METHOD_NOT_ALLOWED
                    || response.status() == StatusCode::NOT_IMPLEMENTED =>
            {
                client.get(url).header(RANGE, "bytes=0-0").send().await?
            }
            Ok(response) => response,
            Err(_) => client.get(url).header(RANGE, "bytes=0-0").send().await?,
        };

        let url_filename = filename_from_url(url);
        let redirect_filename = filename_from_final_url(response.url().as_str())
            .map(|filename| validate_download_filename(&filename))
            .transpose()?
            .filter(|filename| filename != &url_filename);
        let remote_filename = filename_from_content_disposition(&response)
            .map(|filename| validate_download_filename(&filename))
            .transpose()?
            .filter(|filename| filename != &url_filename);

        Ok(ResolvedHttpUrl {
            url: url.to_string(),
            url_filename,
            remote_filename,
            redirect_filename,
            final_url: Some(response.url().to_string()),
        })
    }

    async fn cancel_download(&self, gid: &str, delete_files: bool) -> Result<()> {
        let files = if delete_files {
            let status: Aria2Status = self
                .call(
                    "aria2.tellStatus",
                    vec![
                        json!(gid),
                        json!([
                            "gid",
                            "status",
                            "totalLength",
                            "completedLength",
                            "downloadSpeed",
                            "uploadSpeed",
                            "connections",
                            "errorCode",
                            "errorMessage",
                            "files"
                        ]),
                    ],
                )
                .await?;
            status.files.unwrap_or_default()
        } else {
            Vec::new()
        };
        let _: String = self.call("aria2.forceRemove", vec![json!(gid)]).await?;
        if delete_files {
            let warnings = delete_paths(files).await;
            self.snapshot.write().await.warnings = warnings;
        }
        Ok(())
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T> {
        self.ensure_runtime().await?;
        let runtime = self.runtime.lock().await;
        let runtime = runtime
            .as_ref()
            .ok_or_else(|| eyre!("aria2 runtime missing"))?;
        runtime.rpc.call(method, params).await
    }

    async fn check_child_exit(&self) -> Result<()> {
        let mut runtime = self.runtime.lock().await;
        if let Some(current) = runtime.as_mut() {
            if let Some(status) = current.child.try_wait()? {
                warn!("aria2c exited unexpectedly: {status}");
                {
                    let mut snapshot = self.snapshot.write().await;
                    snapshot.aria2_status.lifecycle = ChildLifecycle::Restarting;
                    snapshot.aria2_status.last_exit = Some(status.to_string());
                    snapshot.aria2_status.restart_count += 1;
                }
                *runtime = None;
                drop(runtime);
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut runtime = self.runtime.lock().await;
                self.spawn_runtime(&mut runtime, ChildLifecycle::Restarting)
                    .await?;
            }
        }
        Ok(())
    }
}

pub async fn run(state: SharedDaemonState) -> Result<()> {
    let mut ticker = tokio::time::interval(Duration::from_millis(
        state.app.config.daemon.poll_interval_ms,
    ));
    loop {
        ticker.tick().await;
        if let Err(error) = state.perform_refresh().await {
            error!("refresh failed: {error:?}");
            let mut snapshot = state.snapshot.write().await;
            snapshot.aria2_status.last_error = Some(error.to_string());
            snapshot.aria2_status.lifecycle = ChildLifecycle::Failed;
        }
    }
}

fn parse_global(global: Aria2GlobalStat) -> GlobalStats {
    GlobalStats {
        download_speed_bps: global.download_speed.parse().unwrap_or(0),
        upload_speed_bps: global.upload_speed.parse().unwrap_or(0),
        num_active: global.num_active.parse().unwrap_or(0),
        num_waiting: global.num_waiting.parse().unwrap_or(0),
        num_stopped: global.num_stopped.parse().unwrap_or(0),
    }
}

fn map_status(status: Aria2Status) -> DownloadItem {
    let total_bytes = status.total_length.parse().unwrap_or(0);
    let completed_bytes = status.completed_length.parse().unwrap_or(0);
    let download_speed_bps = status.download_speed.parse().unwrap_or(0);
    let eta_seconds = if download_speed_bps > 0 && total_bytes >= completed_bytes {
        Some((total_bytes - completed_bytes) / download_speed_bps.max(1))
    } else {
        None
    };
    let primary_path = status
        .files
        .as_ref()
        .and_then(|files| files.iter().find_map(|file| file.path.clone()));
    let source_uri = status
        .files
        .as_ref()
        .and_then(|files| files.iter().find_map(preferred_uri));
    let name = primary_path
        .as_deref()
        .and_then(|path| {
            PathBuf::from(path)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| source_uri.clone().unwrap_or_else(|| status.gid.clone()));

    DownloadItem {
        gid: status.gid,
        status: match status.status.as_str() {
            "active" => DownloadStatus::Active,
            "waiting" => DownloadStatus::Waiting,
            "paused" => DownloadStatus::Paused,
            "complete" => DownloadStatus::Complete,
            "error" => DownloadStatus::Error,
            "removed" => DownloadStatus::Removed,
            _ => DownloadStatus::Unknown,
        },
        name,
        primary_path,
        source_uri,
        total_bytes,
        completed_bytes,
        download_speed_bps,
        upload_speed_bps: status.upload_speed.parse().unwrap_or(0),
        eta_seconds,
        connections: status.connections.and_then(|v| v.parse().ok()),
        error_code: status.error_code,
        error_message: status.error_message,
    }
}

fn preferred_uri(file: &Aria2File) -> Option<String> {
    file.uris
        .as_ref()?
        .iter()
        .find(|uri| uri.status == "used")
        .or_else(|| file.uris.as_ref()?.first())
        .map(|uri| uri.uri.clone())
}

fn filename_from_url(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(|segments| segments.filter(|segment| !segment.is_empty()).last())
                .map(str::to_string)
        })
        .filter(|segment| !segment.trim().is_empty())
        .unwrap_or_else(|| "download".into())
}

fn filename_from_final_url(url: &str) -> Option<String> {
    let filename = filename_from_url(url);
    if filename == "download" {
        None
    } else {
        Some(filename)
    }
}

fn filename_from_content_disposition(response: &reqwest::Response) -> Option<String> {
    let header = response.headers().get(CONTENT_DISPOSITION)?.to_str().ok()?;
    extract_filename_from_content_disposition(header)
}

fn extract_filename_from_content_disposition(header: &str) -> Option<String> {
    for part in header.split(';').map(str::trim) {
        if let Some(value) = part.strip_prefix("filename*=") {
            let value = value.split("''").last().unwrap_or(value);
            let value = value.trim_matches('"').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        if let Some(value) = part.strip_prefix("filename=") {
            let value = value.trim_matches('"').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn validate_download_filename(input: &str) -> Result<String> {
    let filename = input.trim();
    if filename.is_empty() {
        return Err(eyre!("filename cannot be empty"));
    }
    if matches!(filename, "." | "..") {
        return Err(eyre!("filename cannot be '.' or '..'"));
    }
    if filename.contains('/') || filename.contains('\\') || filename.contains('\0') {
        return Err(eyre!("filename must not contain path separators"));
    }
    Ok(filename.to_string())
}

async fn delete_paths(files: Vec<Aria2File>) -> Vec<String> {
    let mut warnings = Vec::new();
    for file in files {
        if let Some(path) = file.path {
            if let Err(error) = tokio::fs::remove_file(&path).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    warnings.push(format!("failed to delete {path}: {error}"));
                }
            }
            let sidecar = format!("{path}.aria2");
            if let Err(error) = tokio::fs::remove_file(&sidecar).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    warnings.push(format!("failed to delete {sidecar}: {error}"));
                }
            }
        }
    }
    warnings
}

fn is_notable_terminal_event(item: &DownloadItem) -> bool {
    matches!(
        item.status,
        DownloadStatus::Complete | DownloadStatus::Error | DownloadStatus::Removed
    )
}

fn event_key(item: &DownloadItem) -> String {
    format!(
        "{}:{:?}:{}",
        item.gid,
        item.status,
        item.error_code.clone().unwrap_or_default()
    )
}

fn webhook_title_for_item(item: &DownloadItem) -> String {
    match item.status {
        DownloadStatus::Complete => "Download completed".into(),
        DownloadStatus::Error => "Download failed".into(),
        DownloadStatus::Removed => "Download removed".into(),
        _ => "Download update".into(),
    }
}

fn webhook_body_for_item(item: &DownloadItem) -> String {
    format!(
        "Status: {}\nName: {}\nGID: {}\nDownloaded: {} / {}\nFinal speed: {}\nPath: {}\nSource: {}\nError code: {}\nError: {}",
        status_name(&item.status),
        item.name,
        item.gid,
        bytes_human(item.completed_bytes),
        bytes_human(item.total_bytes),
        bytes_human_per_sec(item.download_speed_bps),
        item.primary_path.clone().unwrap_or_else(|| "-".into()),
        item.source_uri.clone().unwrap_or_else(|| "-".into()),
        item.error_code.clone().unwrap_or_else(|| "-".into()),
        item.error_message.clone().unwrap_or_else(|| "-".into()),
    )
}

fn status_name(status: &DownloadStatus) -> &'static str {
    match status {
        DownloadStatus::Active => "active",
        DownloadStatus::Waiting => "waiting",
        DownloadStatus::Paused => "paused",
        DownloadStatus::Complete => "complete",
        DownloadStatus::Error => "error",
        DownloadStatus::Removed => "removed",
        DownloadStatus::Unknown => "unknown",
    }
}

async fn post_discord_webhook(
    settings: WebhookSnapshot,
    title: String,
    description: String,
) -> Result<()> {
    let mention = mention_prefix(settings.ping_mode, settings.ping_id.as_deref());
    let content = format!("{mention}**{title}**");
    let body = json!({
        "content": content,
        "allowed_mentions": allowed_mentions_json(settings.ping_mode, settings.ping_id.as_deref()),
        "embeds": [
            {
                "title": title,
                "description": description,
                "color": 0x2ecc71u32,
            }
        ]
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let response = client
        .post(settings.discord_webhook_url)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(eyre!("webhook returned {}", response.status()));
    }
    Ok(())
}

fn allowed_mentions_json(mode: WebhookPingMode, ping_id: Option<&str>) -> Value {
    match mode {
        WebhookPingMode::None => json!({ "parse": [] }),
        WebhookPingMode::Everyone => json!({ "parse": ["everyone"] }),
        WebhookPingMode::SpecificId => {
            let id = ping_id.unwrap_or_default();
            json!({
                "parse": [],
                "users": [id],
                "roles": [id],
            })
        }
    }
}

fn bytes_human(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = value as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", value as u64, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn bytes_human_per_sec(value: u64) -> String {
    format!("{}/s", bytes_human(value))
}

trait IfEmptyThen {
    fn if_empty_then(self, fallback: String) -> String;
}

impl IfEmptyThen for String {
    fn if_empty_then(self, fallback: String) -> String {
        if self.is_empty() { fallback } else { self }
    }
}
