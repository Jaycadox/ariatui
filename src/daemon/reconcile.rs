use std::{
    collections::{HashMap, HashSet},
    path::Path,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Local, Utc};
use color_eyre::eyre::{Result, eyre};
use reqwest::{
    StatusCode,
    header::{CONTENT_DISPOSITION, CONTENT_TYPE, RANGE},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tracing::{error, warn};

use crate::{
    daemon::{
        AppContext, child, queue,
        snapshot::{
            ApiPayload, ApiReply, Aria2ChildStatus, ChildLifecycle, DownloadItem, DownloadStatus,
            GlobalStats, QueueBatchTarget, QueueSnapshot, ResolvedHttpUrl, RoutingSnapshot,
            SchedulerSnapshot, Snapshot, TorrentSettingsSnapshot, WebUiStatus, WebhookSnapshot,
        },
    },
    download_uri::{DownloadUriKind, classify_download_uri, magnet_display_name},
    history,
    routing::{match_rule, normalize_rules},
    rpc::{
        client::Aria2RpcClient,
        types::{Aria2File, Aria2GlobalStat, Aria2Status},
    },
    schedule,
    speed::RollingSpeedTracker,
    state::validate_torrent_size_mib,
    state::{validate_queue_batch, validate_queue_slots},
    torrent_engine::TorrentEngine,
    units, web,
    webhook::{
        WebhookPingMode, mention_prefix, validate_discord_webhook_url, validate_ping_id,
        webhook_enabled,
    },
};

const WEBHOOK_MIN_BYTES: u64 = 20 * 1024 * 1024;
const DAILY_RETRY_AFTER: u32 = 10;
const DAILY_RETRY_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RetryEntry {
    source_uri: String,
    output_path: Option<String>,
    current_gid: String,
    retries: u32,
    next_attempt_at: Option<DateTime<Utc>>,
    daily: bool,
    #[serde(default)]
    batch: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct RetryState {
    entries: HashMap<String, RetryEntry>,
}

impl RetryState {
    fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct RuntimeAria2 {
    pub rpc: Aria2RpcClient,
    pub child: tokio::process::Child,
}

#[derive(Debug)]
pub struct DaemonState {
    pub app: Arc<AppContext>,
    pub runtime: Mutex<Option<RuntimeAria2>>,
    pub torrent_engine: TorrentEngine,
    pub snapshot: RwLock<Snapshot>,
    pub desired_limit_bps: RwLock<Option<u64>>,
    pub desired_slots: RwLock<Option<u8>>,
    pub queue_state: Mutex<queue::QueueState>,
    pub speed_tracker: Mutex<RollingSpeedTracker>,
    pub history_downloads: Mutex<Vec<DownloadItem>>,
    pub log_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub seen_terminal_events: Mutex<HashSet<String>>,
    pub notifications_initialized: Mutex<bool>,
    retry_state: Mutex<RetryState>,
    daily_failure_events: Mutex<HashSet<String>>,
    pub last_notified_restart_count: Mutex<u32>,
    pub web_pairings: Mutex<HashMap<String, WebPairing>>,
    pub web_sessions: Mutex<HashMap<String, Instant>>,
    pub web_revoked_sessions: Mutex<HashMap<String, Instant>>,
    pub qbt_categories: Mutex<HashMap<String, String>>,
    pub cli_idempotency: Mutex<HashMap<String, String>>,
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
        let history_downloads = history::load(&app.paths.history_file)?;
        let torrent_engine =
            TorrentEngine::new(&app.paths, expand_tilde(&app.config.daemon.download_dir)).await?;
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
        let retry_state = RetryState::load(&app.paths.retry_state_file);
        let queue_state = queue::QueueState::load(&app.paths.queue_state_file);
        Ok(Self {
            app,
            runtime: Mutex::new(None),
            torrent_engine,
            snapshot: RwLock::new(snapshot),
            desired_limit_bps: RwLock::new(None),
            desired_slots: RwLock::new(None),
            queue_state: Mutex::new(queue_state),
            speed_tracker: Mutex::new(RollingSpeedTracker::default()),
            history_downloads: Mutex::new(history_downloads),
            log_task: Mutex::new(None),
            seen_terminal_events: Mutex::new(HashSet::new()),
            notifications_initialized: Mutex::new(false),
            retry_state: Mutex::new(retry_state),
            daily_failure_events: Mutex::new(HashSet::new()),
            last_notified_restart_count: Mutex::new(0),
            web_pairings: Mutex::new(HashMap::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web_revoked_sessions: Mutex::new(HashMap::new()),
            qbt_categories: Mutex::new(HashMap::new()),
            cli_idempotency: Mutex::new(HashMap::new()),
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
        let slots = self.app.state.read().await.queue_slots;
        let (child_process, rx) = child::spawn_aria2(
            &self.app.config,
            self.app.paths.aria2_session_file.clone(),
            slots,
        )
        .await?;
        *self.desired_slots.write().await = Some(slots);

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
        let refresh_started = Instant::now();
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
                    "infoHash",
                    "numSeeders",
                    "followedBy",
                    "belongsTo",
                    "files",
                    "bittorrent"
                ])],
            )
            .await?;
        // Batch enforcement must see the entire waiting queue. Treat the
        // configured limit as an RPC page size; otherwise downloads beyond the
        // first page could start without ever being held for their batch.
        let waiting_page_size = self.app.config.daemon.waiting_limit.max(1);
        let mut waiting = Vec::new();
        let mut waiting_offset = 0usize;
        loop {
            let page: Vec<Aria2Status> = runtime
                .rpc
                .call(
                    "aria2.tellWaiting",
                    vec![
                        json!(waiting_offset),
                        json!(waiting_page_size),
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
                            "infoHash",
                            "numSeeders",
                            "followedBy",
                            "belongsTo",
                            "files",
                            "bittorrent"
                        ]),
                    ],
                )
                .await?;
            let page_len = page.len();
            waiting.extend(page);
            if page_len < waiting_page_size {
                break;
            }
            waiting_offset = waiting_offset.saturating_add(page_len);
        }
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
                        "infoHash",
                        "numSeeders",
                        "followedBy",
                        "belongsTo",
                        "files",
                        "bittorrent"
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
            self.torrent_engine
                .apply_download_limit(resolved.effective_limit_bps);
            *desired_limit = resolved.effective_limit_bps;
        }
        let torrent_downloads = self.torrent_engine.snapshot();
        let torrent_terminal_downloads = TorrentEngine::terminal_download_items(&torrent_downloads);
        let stopped_downloads = stopped.into_iter().map(map_status).collect::<Vec<_>>();
        let mut history_downloads = {
            let mut history_downloads = self.history_downloads.lock().await;
            let merged = history::merge_terminal_events(
                &history_downloads,
                stopped_downloads
                    .into_iter()
                    .chain(torrent_terminal_downloads),
            );
            if merged != *history_downloads {
                history::save(&self.app.paths.history_file, &merged)?;
                *history_downloads = merged;
            }
            history_downloads.clone()
        };
        let mut current_downloads: Vec<DownloadItem> =
            active.into_iter().chain(waiting).map(map_status).collect();
        current_downloads.extend(TorrentEngine::current_download_items(&torrent_downloads));
        {
            let queue_state = self.queue_state.lock().await;
            queue::attach_metadata(&queue_state, &mut current_downloads);
            queue::attach_metadata(&queue_state, &mut history_downloads);
        }
        let queue_summary = self
            .apply_queue_policy(runtime, &mut current_downloads)
            .await;

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
        snapshot.torrents = TorrentSettingsSnapshot {
            mode: state.torrent_streaming_mode,
            head_size_mib: state.torrent_head_size_mib,
            tail_size_mib: state.torrent_tail_size_mib,
            aria2_prioritize_piece: state.torrent_prioritize_piece_value()?,
            engine: "librqbit".into(),
            downloads: torrent_downloads.clone(),
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
        snapshot.queue = queue_summary;
        self.speed_tracker
            .lock()
            .await
            .refresh(refresh_started, &mut current_downloads);
        // Keep the aggregate surface as steady as the per-download rows. The
        // sum also avoids a second, independently jittery aria2 estimator.
        snapshot.global.download_speed_bps = current_downloads
            .iter()
            .filter(|item| item.status == DownloadStatus::Active)
            .fold(0u64, |total, item| {
                total.saturating_add(item.download_speed_bps)
            });
        snapshot.current_downloads = current_downloads;
        snapshot.history_downloads = history_downloads;
        let snapshot_copy = snapshot.clone();
        drop(snapshot);
        self.prune_queue_state(&snapshot_copy).await;
        self.process_download_retries(runtime, &snapshot_copy).await;
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

    async fn process_download_retries(&self, runtime: &RuntimeAria2, snapshot: &Snapshot) {
        let now = Utc::now();
        let mut state = self.retry_state.lock().await;
        let mut changed = false;

        changed |= remove_completed_retry_entries(&mut state, &snapshot.history_downloads);

        for item in snapshot
            .history_downloads
            .iter()
            .filter(|item| item.status == DownloadStatus::Error)
        {
            let Some(source_uri) = item.source_uri.clone() else {
                continue;
            };
            let key = retry_key(&source_uri, item.primary_path.as_deref());
            let entry = state.entries.entry(key).or_insert_with(|| RetryEntry {
                source_uri,
                output_path: item.primary_path.clone(),
                current_gid: item.gid.clone(),
                retries: 0,
                next_attempt_at: None,
                daily: false,
                batch: item.batch,
            });
            if entry.batch.is_none() && item.batch.is_some() {
                entry.batch = item.batch;
                changed = true;
            }
            if entry.current_gid != item.gid || entry.next_attempt_at.is_some() {
                continue;
            }

            if entry.retries > DAILY_RETRY_AFTER && !entry.daily {
                entry.daily = true;
                self.daily_failure_events
                    .lock()
                    .await
                    .insert(item.gid.clone());
            }
            let delay = if entry.daily {
                DAILY_RETRY_SECS
            } else {
                retry_delay_secs(entry.retries)
            };
            entry.next_attempt_at = Some(now + chrono::Duration::seconds(delay));
            changed = true;
        }

        let due_keys = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.next_attempt_at.is_some_and(|due| due <= now))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in due_keys {
            let Some(entry) = state.entries.get(&key).cloned() else {
                continue;
            };
            let options = retry_options(&entry);
            match runtime
                .rpc
                .call::<String>("aria2.addUri", vec![json!([entry.source_uri]), options])
                .await
            {
                Ok(gid) => {
                    let batch = state.entries.get(&key).and_then(|entry| entry.batch);
                    if let Some(current) = state.entries.get_mut(&key) {
                        current.current_gid = gid.clone();
                        current.retries = current.retries.saturating_add(1);
                        current.next_attempt_at = None;
                    }
                    self.remember_download_batch(&gid, batch).await;
                    let _ = runtime
                        .rpc
                        .call::<String>(
                            "aria2.removeDownloadResult",
                            vec![json!(entry.current_gid)],
                        )
                        .await;
                }
                Err(error) => {
                    warn!("failed to submit scheduled retry: {error}");
                    if let Some(current) = state.entries.get_mut(&key) {
                        current.next_attempt_at = Some(now + chrono::Duration::seconds(60));
                    }
                }
            }
            changed = true;
        }

        if changed && let Err(error) = state.save(&self.app.paths.retry_state_file) {
            warn!("failed to persist download retry state: {error}");
        }
    }

    async fn forget_download_retry(&self, gid: &str) {
        let mut state = self.retry_state.lock().await;
        let before = state.entries.len();
        state.entries.retain(|_, entry| entry.current_gid != gid);
        if before != state.entries.len()
            && let Err(error) = state.save(&self.app.paths.retry_state_file)
        {
            warn!("failed to persist download retry state: {error}");
        }
    }

    async fn forget_download_queue(&self, gid: &str) {
        let mut queue = self.queue_state.lock().await;
        let before = queue.len();
        queue.forget(gid);
        if before != queue.len()
            && let Err(error) = queue.save(&self.app.paths.queue_state_file)
        {
            warn!("failed to persist queue state: {error}");
        }
    }

    /// Mark a download as paused by the user rather than by the batch scheduler.
    async fn user_pause_download(&self, gid: &str) {
        let mut queue = self.queue_state.lock().await;
        if queue.is_held(gid) {
            queue.set_held(gid, false);
            if let Err(error) = queue.save(&self.app.paths.queue_state_file) {
                warn!("failed to persist queue state: {error}");
            }
        }
    }

    async fn batch_target_of(&self, gid: &str) -> QueueBatchTarget {
        let queue = self.queue_state.lock().await;
        QueueBatchTarget::of(queue.batch(gid))
    }

    async fn clear_download_retries(&self) {
        let mut state = self.retry_state.lock().await;
        if state.entries.is_empty() {
            return;
        }
        state.entries.clear();
        if let Err(error) = state.save(&self.app.paths.retry_state_file) {
            warn!("failed to persist download retry state: {error}");
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

    async fn apply_queue_slots(&self, runtime: &RuntimeAria2, slots: u8) -> Result<()> {
        let mut desired = self.desired_slots.write().await;
        if *desired == Some(slots) {
            return Ok(());
        }
        let _: String = runtime
            .rpc
            .call(
                "aria2.changeGlobalOption",
                vec![json!({ "max-concurrent-downloads": slots.to_string() })],
            )
            .await?;
        *desired = Some(slots);
        Ok(())
    }

    /// Attach batch bookkeeping to what aria2 just reported, keep the batch
    /// policy enforced, and return the summary the UI surfaces show.
    async fn apply_queue_policy(
        &self,
        runtime: &RuntimeAria2,
        items: &mut [DownloadItem],
    ) -> QueueSnapshot {
        let slots = self.app.state.read().await.queue_slots;
        if let Err(error) = self.apply_queue_slots(runtime, slots).await {
            warn!("failed to update aria2 download slots: {error}");
        }
        let (active_batch, actions) = queue::plan_batch_policy(items);
        let (applied, _) = self.apply_queue_actions(runtime, &actions).await;
        for (gid, held) in applied {
            if let Some(item) = items.iter_mut().find(|item| item.gid == gid) {
                item.queue_held = held;
                if held {
                    item.status = DownloadStatus::Paused;
                } else if item.status == DownloadStatus::Paused {
                    item.status = DownloadStatus::Waiting;
                }
            }
        }
        queue::summarize_queue(slots, active_batch, items)
    }

    /// Runs scheduler decisions against aria2 and returns `(gid, held)` for the
    /// ones that succeeded so callers can update their own view immediately.
    async fn apply_queue_actions(
        &self,
        runtime: &RuntimeAria2,
        actions: &[queue::QueueAction],
    ) -> (Vec<(String, bool)>, Vec<String>) {
        let mut applied = Vec::new();
        let mut failures = Vec::new();
        if actions.is_empty() {
            return (applied, failures);
        }
        let mut queue = self.queue_state.lock().await;
        for action in actions {
            if let queue::QueueAction::KeepPausedByUser { gid } = action {
                queue.set_held(gid, false);
                applied.push((gid.clone(), false));
                continue;
            }
            let (method, gid, held) = match action {
                queue::QueueAction::Hold { gid } => ("aria2.forcePause", gid.as_str(), true),
                queue::QueueAction::HoldByUser { gid } => ("aria2.forcePause", gid.as_str(), false),
                queue::QueueAction::Release { gid } => ("aria2.unpause", gid.as_str(), false),
                queue::QueueAction::KeepPausedByUser { .. } => unreachable!(),
            };
            let result = if TorrentEngine::is_torrent_gid(gid) {
                if method == "aria2.unpause" {
                    self.torrent_engine.resume(gid).await
                } else {
                    self.torrent_engine.pause(gid).await
                }
            } else {
                runtime
                    .rpc
                    .call::<String>(method, vec![json!(gid)])
                    .await
                    .map(|_| ())
            };
            match result {
                Ok(_) => {
                    queue.set_held(gid, held);
                    applied.push((gid.to_string(), held));
                }
                Err(error) => {
                    let message = format!("failed to {method} queued download {gid}: {error}");
                    warn!("{message}");
                    failures.push(message);
                }
            }
        }
        if !applied.is_empty()
            && let Err(error) = queue.save(&self.app.paths.queue_state_file)
        {
            warn!("failed to persist queue state: {error}");
        }
        (applied, failures)
    }

    async fn apply_queue_request(&self, actions: Vec<queue::QueueAction>) -> Result<()> {
        if actions.is_empty() {
            return Ok(());
        }
        self.ensure_runtime().await?;
        let runtime = self.runtime.lock().await;
        let runtime = runtime
            .as_ref()
            .ok_or_else(|| eyre!("aria2 runtime missing"))?;
        let (_, failures) = self.apply_queue_actions(runtime, &actions).await;
        if !failures.is_empty() {
            return Err(eyre!(failures.join("; ")));
        }
        Ok(())
    }

    async fn hold_queue_batch(&self, target: QueueBatchTarget) -> Result<()> {
        let items = self.snapshot.read().await.current_downloads.clone();
        self.apply_queue_request(queue::plan_hold_batch(&items, target))
            .await
    }

    async fn start_queue_batch(&self, target: QueueBatchTarget) -> Result<()> {
        let items = self.snapshot.read().await.current_downloads.clone();
        self.apply_queue_request(queue::plan_start_batch(&items, target))
            .await
    }

    async fn set_download_batch(&self, gid: &str, batch: Option<u32>) -> Result<()> {
        let batch = validate_queue_batch(batch)?;
        let mut queue = self.queue_state.lock().await;
        queue.set_batch(gid, batch);
        queue.save(&self.app.paths.queue_state_file)
    }

    async fn remember_download_batch(&self, gid: &str, batch: Option<u32>) {
        if batch.is_none() {
            return;
        }
        let mut queue = self.queue_state.lock().await;
        queue.set_batch(gid, batch);
        if let Err(error) = queue.save(&self.app.paths.queue_state_file) {
            warn!("failed to persist queue state: {error}");
        }
    }

    async fn prune_queue_state(&self, snapshot: &Snapshot) {
        let live = snapshot
            .current_downloads
            .iter()
            .chain(snapshot.history_downloads.iter())
            .flat_map(|item| [Some(item.gid.as_str()), item.belongs_to.as_deref()])
            .flatten()
            .collect::<HashSet<_>>();
        let mut queue = self.queue_state.lock().await;
        if queue.retain(|gid| live.contains(gid))
            && let Err(error) = queue.save(&self.app.paths.queue_state_file)
        {
            warn!("failed to persist queue state: {error}");
        }
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
            crate::daemon::ApiRequest::AddHttpUrl {
                url,
                filename,
                batch,
            } => {
                let batch = validate_queue_batch(batch)?;
                let state = self.app.state.read().await.clone();
                let uri_kind = classify_download_uri(&url)?;
                let effective_limit_bps =
                    schedule::resolve(Local::now(), &state)?.effective_limit_bps;
                if matches!(uri_kind, DownloadUriKind::HttpLike)
                    && self.try_add_remote_torrent(&url, &state, batch).await?
                {
                    self.save_aria2_session().await;
                    self.perform_refresh().await?;
                    return Ok(ApiReply {
                        snapshot: self.snapshot().await,
                        payload,
                    });
                }
                let routing_name = match uri_kind {
                    DownloadUriKind::Magnet => filename
                        .clone()
                        .or_else(|| magnet_display_name(&url))
                        .unwrap_or_else(|| "torrent".into()),
                    DownloadUriKind::HttpLike => {
                        filename.clone().unwrap_or_else(|| filename_from_url(&url))
                    }
                };
                let route = match_rule(
                    &state.default_download_dir,
                    &state.download_rules,
                    &routing_name,
                )?;
                tokio::fs::create_dir_all(&route.resolved_directory).await?;
                if matches!(uri_kind, DownloadUriKind::Magnet) {
                    let gid = self
                        .torrent_engine
                        .add_url(
                            &url,
                            route.resolved_directory,
                            effective_limit_bps,
                            state.torrent_streaming_mode != crate::state::TorrentStreamingMode::Off,
                        )
                        .await?;
                    self.remember_download_batch(&gid, batch).await;
                    self.perform_refresh().await?;
                    return Ok(ApiReply {
                        snapshot: self.snapshot().await,
                        payload,
                    });
                }
                let filename = validate_download_filename(
                    filename.unwrap_or_else(|| filename_from_url(&url)).trim(),
                )?;
                let options = json!({
                    "dir": route.resolved_directory.display().to_string(),
                    "out": filename,
                });
                let gid: String = self
                    .call("aria2.addUri", vec![json!([url]), options])
                    .await?;
                self.remember_download_batch(&gid, batch).await;
                self.save_aria2_session().await;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::AddDownload {
                url,
                filename,
                directory,
                options,
                idempotency_key,
            } => {
                if let Some(key) = idempotency_key.as_deref()
                    && (key.trim().is_empty() || key.len() > 200)
                {
                    return Err(eyre!("idempotency key must contain 1 to 200 characters"));
                }
                if let Some(key) = idempotency_key.as_deref()
                    && let Some(gid) = self.cli_idempotency.lock().await.get(key).cloned()
                {
                    let snapshot = self.snapshot().await;
                    if let Some(item) = snapshot
                        .current_downloads
                        .iter()
                        .chain(snapshot.history_downloads.iter())
                        .find(|item| item.gid == gid)
                        .cloned()
                    {
                        return Ok(ApiReply {
                            snapshot,
                            payload: Some(ApiPayload::Download {
                                download: item,
                                created: false,
                            }),
                        });
                    }
                }
                let state = self.app.state.read().await.clone();
                let uri_kind = classify_download_uri(&url)?;
                let routing_name = match uri_kind {
                    DownloadUriKind::Magnet => filename
                        .clone()
                        .or_else(|| magnet_display_name(&url))
                        .unwrap_or_else(|| "torrent".into()),
                    DownloadUriKind::HttpLike => {
                        filename.clone().unwrap_or_else(|| filename_from_url(&url))
                    }
                };
                let output_dir = if let Some(directory) = directory {
                    crate::routing::validate_directory_input(&directory)?
                } else {
                    match_rule(
                        &state.default_download_dir,
                        &state.download_rules,
                        &routing_name,
                    )?
                    .resolved_directory
                };
                tokio::fs::create_dir_all(&output_dir).await?;
                let effective_limit_bps =
                    schedule::resolve(Local::now(), &state)?.effective_limit_bps;
                let remote_torrent = matches!(uri_kind, DownloadUriKind::HttpLike)
                    && (url.starts_with("http://") || url.starts_with("https://"))
                    && self
                        .resolve_http_url(&url)
                        .await
                        .map(|resolved| resolved.is_torrent)
                        .unwrap_or(false);
                let gid = if matches!(uri_kind, DownloadUriKind::Magnet) || remote_torrent {
                    let paused = options.get("pause").is_some_and(|value| value == "true");
                    if options.keys().any(|key| key != "pause") {
                        return Err(eyre!(
                            "aria2 options are not supported for torrent downloads"
                        ));
                    }
                    let gid = self
                        .torrent_engine
                        .add_url(
                            &url,
                            output_dir,
                            effective_limit_bps,
                            state.torrent_streaming_mode != crate::state::TorrentStreamingMode::Off,
                        )
                        .await?;
                    if paused {
                        self.torrent_engine.pause(&gid).await?;
                    }
                    gid
                } else {
                    let filename = validate_download_filename(
                        filename.unwrap_or_else(|| filename_from_url(&url)).trim(),
                    )?;
                    let mut rpc_options = serde_json::Map::new();
                    for (key, value) in options {
                        validate_aria2_option_name(&key)?;
                        if matches!(key.as_str(), "dir" | "out") {
                            return Err(eyre!(
                                "aria2 option '{key}' is managed by --dir/--output-name"
                            ));
                        }
                        rpc_options.insert(key, json!(value));
                    }
                    rpc_options.insert("dir".into(), json!(output_dir.display().to_string()));
                    rpc_options.insert("out".into(), json!(filename));
                    self.call::<String>(
                        "aria2.addUri",
                        vec![json!([url]), Value::Object(rpc_options)],
                    )
                    .await?
                };
                self.save_aria2_session().await;
                self.perform_refresh().await?;
                let snapshot = self.snapshot().await;
                if let Some(key) = idempotency_key {
                    self.cli_idempotency.lock().await.insert(key, gid.clone());
                }
                if let Some(item) = snapshot
                    .current_downloads
                    .iter()
                    .chain(snapshot.history_downloads.iter())
                    .find(|item| item.gid == gid)
                    .cloned()
                {
                    payload = Some(ApiPayload::Download {
                        download: item,
                        created: true,
                    });
                }
            }
            crate::daemon::ApiRequest::Pause { gid, force } => {
                if TorrentEngine::is_torrent_gid(&gid) {
                    self.torrent_engine.pause(&gid).await?;
                    self.user_pause_download(&gid).await;
                    self.perform_refresh().await?;
                    return Ok(ApiReply {
                        snapshot: self.snapshot().await,
                        payload,
                    });
                }
                let method = if force {
                    "aria2.forcePause"
                } else {
                    "aria2.pause"
                };
                let _: String = self.call(method, vec![json!(gid)]).await?;
                self.user_pause_download(&gid).await;
                self.save_aria2_session().await;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::Resume { gid } => {
                let held = self.queue_state.lock().await.is_held(&gid);
                if held {
                    // Resuming a file the scheduler held means the user wants
                    // that whole batch now, so hand it the turn.
                    let target = self.batch_target_of(&gid).await;
                    self.start_queue_batch(target).await?;
                } else if TorrentEngine::is_torrent_gid(&gid) {
                    self.torrent_engine.resume(&gid).await?;
                } else {
                    let _: String = self.call("aria2.unpause", vec![json!(gid)]).await?;
                    self.save_aria2_session().await;
                }
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::SetDownloadBatch { gid, batch } => {
                self.set_download_batch(&gid, batch).await?;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::HoldQueueBatch { target } => {
                let target = QueueBatchTarget::of(validate_queue_batch(target.batch())?);
                self.hold_queue_batch(target).await?;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::StartQueueBatch { target } => {
                let target = QueueBatchTarget::of(validate_queue_batch(target.batch())?);
                self.start_queue_batch(target).await?;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::SetQueueSlots { slots } => {
                let slots = validate_queue_slots(slots)?;
                let mut state = self.app.state.write().await;
                state.queue_slots = slots;
                state.save(&self.app.paths.state_file)?;
                drop(state);
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::Cancel { gid, delete_files } => {
                if TorrentEngine::is_torrent_gid(&gid) {
                    self.torrent_engine.cancel(&gid, delete_files).await?;
                    self.forget_download_retry(&gid).await;
                    self.forget_download_queue(&gid).await;
                    self.perform_refresh().await?;
                    return Ok(ApiReply {
                        snapshot: self.snapshot().await,
                        payload,
                    });
                }
                self.cancel_download(&gid, delete_files).await?;
                self.forget_download_retry(&gid).await;
                self.save_aria2_session().await;
                self.forget_download_queue(&gid).await;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::RemoveHistory { gid } => {
                {
                    let mut history_downloads = self.history_downloads.lock().await;
                    if history::remove(&mut history_downloads, &gid) {
                        history::save(&self.app.paths.history_file, &history_downloads)?;
                    }
                }
                if !TorrentEngine::is_torrent_gid(&gid) {
                    if let Err(error) = self
                        .call::<String>("aria2.removeDownloadResult", vec![json!(gid)])
                        .await
                    {
                        warn!("failed to remove aria2 history item: {error}");
                    }
                    self.save_aria2_session().await;
                }
                self.forget_download_retry(&gid).await;
                self.forget_download_queue(&gid).await;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::ChangePosition { gid, offset } => {
                let _: String = self
                    .call(
                        "aria2.changePosition",
                        vec![json!(gid), json!(offset), json!("POS_CUR")],
                    )
                    .await?;
                self.save_aria2_session().await;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::PauseAll => {
                let _: String = self.call("aria2.pauseAll", vec![]).await?;
                for item in TorrentEngine::current_download_items(&self.torrent_engine.snapshot())
                    .into_iter()
                    .filter(|item| {
                        matches!(
                            item.status,
                            DownloadStatus::Active | DownloadStatus::Waiting
                        )
                    })
                {
                    self.torrent_engine.pause(&item.gid).await?;
                }
                self.save_aria2_session().await;
                {
                    let mut queue = self.queue_state.lock().await;
                    if queue.clear_held()
                        && let Err(error) = queue.save(&self.app.paths.queue_state_file)
                    {
                        warn!("failed to persist queue state: {error}");
                    }
                }
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::ResumeAll => {
                let _: String = self.call("aria2.unpauseAll", vec![]).await?;
                for item in TorrentEngine::current_download_items(&self.torrent_engine.snapshot())
                    .into_iter()
                    .filter(|item| item.status == DownloadStatus::Paused)
                {
                    self.torrent_engine.resume(&item.gid).await?;
                }
                self.save_aria2_session().await;
                self.perform_refresh().await?;
            }
            crate::daemon::ApiRequest::PurgeHistory => {
                {
                    let mut history_downloads = self.history_downloads.lock().await;
                    history_downloads.clear();
                    history::save(&self.app.paths.history_file, &history_downloads)?;
                }
                if let Err(error) = self
                    .call::<String>("aria2.purgeDownloadResult", vec![])
                    .await
                {
                    warn!("failed to purge aria2 history: {error}");
                }
                self.clear_download_retries().await;
                self.save_aria2_session().await;
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
            crate::daemon::ApiRequest::SetTorrentStreamingSettings {
                mode,
                head_size_mib,
                tail_size_mib,
            } => {
                validate_torrent_size_mib(head_size_mib, "torrent head size")?;
                validate_torrent_size_mib(tail_size_mib, "torrent tail size")?;
                let mut state = self.app.state.write().await;
                state.torrent_streaming_mode = mode;
                state.torrent_head_size_mib = head_size_mib;
                state.torrent_tail_size_mib = tail_size_mib;
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
            crate::daemon::ApiRequest::RevokeAllWebUiSessions => {
                let tokens = self
                    .web_sessions
                    .lock()
                    .await
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                for token in tokens {
                    web::revoke_session(self, &token).await;
                }
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
            self.daily_failure_events.lock().await.clear();
            return;
        }

        let mut initialized = self.notifications_initialized.lock().await;
        let mut seen = self.seen_terminal_events.lock().await;
        let daily_failures = self.daily_failure_events.lock().await.clone();
        if !*initialized {
            for item in &snapshot.history_downloads {
                if !daily_failures.contains(&item.gid) {
                    seen.insert(event_key(item));
                }
            }
            *self.last_notified_restart_count.lock().await = snapshot.aria2_status.restart_count;
            *initialized = true;
        }

        let new_events = snapshot
            .history_downloads
            .iter()
            .filter(|item| is_notable_terminal_event(item))
            .filter(|item| {
                item.status != DownloadStatus::Error || daily_failures.contains(&item.gid)
            })
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
        self.daily_failure_events.lock().await.clear();

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
        let is_torrent = is_torrent_target(
            Some(&url_filename),
            remote_filename.as_deref(),
            redirect_filename.as_deref(),
            response.url().as_str(),
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
        );

        Ok(ResolvedHttpUrl {
            url: url.to_string(),
            url_filename,
            remote_filename,
            redirect_filename,
            final_url: Some(response.url().to_string()),
            is_torrent,
        })
    }

    async fn try_add_remote_torrent(
        &self,
        url: &str,
        state: &crate::state::PersistedState,
        batch: Option<u32>,
    ) -> Result<bool> {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(false);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(Duration::from_secs(
                self.app.config.daemon.rpc_request_timeout_secs.max(2),
            ))
            .build()?;

        let head = match client.head(url).send().await {
            Ok(response)
                if response.status() == StatusCode::METHOD_NOT_ALLOWED
                    || response.status() == StatusCode::NOT_IMPLEMENTED =>
            {
                None
            }
            Ok(response) => Some(response),
            Err(_) => None,
        };

        let should_treat_as_torrent = if let Some(response) = head.as_ref() {
            let url_filename = filename_from_url(url);
            let redirect_filename = filename_from_final_url(response.url().as_str());
            let remote_filename = filename_from_content_disposition(response);
            is_torrent_target(
                Some(&url_filename),
                remote_filename.as_deref(),
                redirect_filename.as_deref(),
                response.url().as_str(),
                response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
            )
        } else {
            is_torrent_name(url)
        };

        if !should_treat_as_torrent {
            return Ok(false);
        }

        let routing_name = if let Some(response) = head.as_ref() {
            filename_from_content_disposition(response)
                .or_else(|| filename_from_final_url(response.url().as_str()))
                .unwrap_or_else(|| filename_from_url(url))
        } else {
            filename_from_url(url)
        };
        let route = match_rule(
            &state.default_download_dir,
            &state.download_rules,
            &routing_name,
        )?;
        let effective_limit_bps = schedule::resolve(Local::now(), state)?.effective_limit_bps;
        let gid = self
            .torrent_engine
            .add_url(
                url,
                route.resolved_directory,
                effective_limit_bps,
                state.torrent_streaming_mode != crate::state::TorrentStreamingMode::Off,
            )
            .await?;
        self.remember_download_batch(&gid, batch).await;
        Ok(true)
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
                            "infoHash",
                            "numSeeders",
                            "followedBy",
                            "belongsTo",
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

    async fn save_aria2_session(&self) {
        if let Err(error) = self.call::<String>("aria2.saveSession", vec![]).await {
            warn!("failed to save aria2 session: {error}");
        }
    }

    async fn check_child_exit(&self) -> Result<()> {
        let mut runtime = self.runtime.lock().await;
        if let Some(current) = runtime.as_mut()
            && let Some(status) = current.child.try_wait()?
        {
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
    let bittorrent_name = bittorrent_name(status._bittorrent.as_ref());
    let primary_path = status
        .files
        .as_ref()
        .and_then(|files| files.iter().find_map(|file| file.path.clone()));
    let source_uri = status
        .files
        .as_ref()
        .and_then(|files| files.iter().find_map(preferred_uri));
    let torrent_source_uri = status
        .info_hash
        .as_ref()
        .map(|info_hash| format!("magnet:?xt=urn:btih:{info_hash}"));
    let followed_by = status.followed_by.unwrap_or_default();
    let is_metadata_only = total_bytes == 0
        && !followed_by.is_empty()
        && source_uri
            .as_deref()
            .is_some_and(|uri| uri.starts_with("magnet:"));
    let name = bittorrent_name
        .or_else(|| {
            primary_path.as_deref().and_then(|path| {
                PathBuf::from(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
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
        source_uri: source_uri.or(torrent_source_uri),
        info_hash: status.info_hash,
        num_seeders: status.num_seeders.and_then(|v| v.parse().ok()),
        followed_by,
        belongs_to: status.belongs_to,
        is_metadata_only,
        total_bytes,
        completed_bytes,
        download_speed_bps,
        realtime_download_speed_bps: download_speed_bps,
        upload_speed_bps: status.upload_speed.parse().unwrap_or(0),
        eta_seconds,
        connections: status.connections.and_then(|v| v.parse().ok()),
        error_code: status.error_code,
        error_message: status.error_message,
        batch: None,
        queue_held: false,
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

fn bittorrent_name(value: Option<&Value>) -> Option<String> {
    value?
        .get("info")?
        .get("name")?
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn filename_from_url(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
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

fn is_torrent_name(value: &str) -> bool {
    let path = if let Ok(url) = reqwest::Url::parse(value) {
        url.path().to_string()
    } else {
        value.to_string()
    };
    Path::new(&path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
}

fn is_torrent_target(
    url_filename: Option<&str>,
    remote_filename: Option<&str>,
    redirect_filename: Option<&str>,
    final_url: &str,
    content_type: Option<&str>,
) -> bool {
    url_filename.is_some_and(is_torrent_name)
        || remote_filename.is_some_and(is_torrent_name)
        || redirect_filename.is_some_and(is_torrent_name)
        || is_torrent_name(final_url)
        || content_type
            .map(|value| value.to_ascii_lowercase())
            .is_some_and(|value| {
                value.contains("application/x-bittorrent")
                    || value.contains("application/x-torrent")
            })
}

fn expand_tilde(value: &str) -> PathBuf {
    if let Some(stripped) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(stripped);
    }
    PathBuf::from(value)
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

fn validate_aria2_option_name(input: &str) -> Result<()> {
    if input.is_empty()
        || !input
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(eyre!("invalid aria2 option name '{input}'"));
    }
    Ok(())
}

async fn delete_paths(files: Vec<Aria2File>) -> Vec<String> {
    let mut warnings = Vec::new();
    for file in files {
        if let Some(path) = file.path {
            if let Err(error) = tokio::fs::remove_file(&path).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                warnings.push(format!("failed to delete {path}: {error}"));
            }
            let sidecar = format!("{path}.aria2");
            if let Err(error) = tokio::fs::remove_file(&sidecar).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                warnings.push(format!("failed to delete {sidecar}: {error}"));
            }
        }
    }
    warnings
}

fn retry_delay_secs(retries: u32) -> i64 {
    (60_i64.saturating_mul(1_i64 << retries.min(6))).min(60 * 60)
}

fn remove_completed_retry_entries(state: &mut RetryState, history: &[DownloadItem]) -> bool {
    let completed_gids = history
        .iter()
        .filter(|item| item.status == DownloadStatus::Complete)
        .map(|item| item.gid.as_str())
        .collect::<HashSet<_>>();
    let completed_downloads = history
        .iter()
        .filter(|item| item.status == DownloadStatus::Complete)
        .filter_map(history::download_identity)
        .collect::<HashSet<_>>();
    let before = state.entries.len();
    state.entries.retain(|key, entry| {
        !completed_gids.contains(entry.current_gid.as_str()) && !completed_downloads.contains(key)
    });
    before != state.entries.len()
}

fn retry_key(source_uri: &str, output_path: Option<&str>) -> String {
    history::download_identity_parts(source_uri, output_path)
}

fn retry_options(entry: &RetryEntry) -> Value {
    let Some(path) = entry.output_path.as_deref().map(Path::new) else {
        return json!({});
    };
    let mut options = serde_json::Map::new();
    if let Some(parent) = path.parent() {
        options.insert("dir".into(), json!(parent.display().to_string()));
    }
    if !entry.source_uri.starts_with("magnet:")
        && let Some(name) = path.file_name()
    {
        options.insert("out".into(), json!(name.to_string_lossy()));
    }
    Value::Object(options)
}

fn is_notable_terminal_event(item: &DownloadItem) -> bool {
    if item.is_metadata_only {
        return false;
    }
    if item.total_bytes.max(item.completed_bytes) < WEBHOOK_MIN_BYTES {
        return false;
    }
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

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::config::AppConfig;

    fn history_item(gid: &str, status: DownloadStatus) -> DownloadItem {
        DownloadItem {
            gid: gid.into(),
            status,
            name: "release.iso".into(),
            primary_path: Some("/downloads/release.iso".into()),
            source_uri: Some("https://example.com/release.iso".into()),
            info_hash: None,
            num_seeders: None,
            followed_by: Vec::new(),
            belongs_to: None,
            is_metadata_only: false,
            total_bytes: 100,
            completed_bytes: 100,
            download_speed_bps: 0,
            realtime_download_speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: None,
            error_code: None,
            error_message: None,
            batch: None,
            queue_held: false,
        }
    }

    #[test]
    fn retry_backoff_grows_and_caps_at_one_hour() {
        assert_eq!(retry_delay_secs(0), 60);
        assert_eq!(retry_delay_secs(1), 120);
        assert_eq!(retry_delay_secs(5), 1_920);
        assert_eq!(retry_delay_secs(6), 3_600);
        assert_eq!(retry_delay_secs(50), 3_600);
    }

    #[tokio::test]
    async fn batch_assignments_persist_and_validate() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ariatui-queue-tests-{nonce}"));
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        let runtime_dir = root.join("runtime");
        let paths = crate::paths::AppPaths {
            config_dir: config_dir.clone(),
            state_dir: state_dir.clone(),
            runtime_dir: runtime_dir.clone(),
            config_file: config_dir.join("config.toml"),
            state_file: state_dir.join("state.toml"),
            socket_path: runtime_dir.join("daemon.sock"),
            daemon_marker_file: runtime_dir.join(".daemon"),
            snapshot_cache_file: runtime_dir.join(".snapshot"),
            history_file: state_dir.join("history.json"),
            torrent_session_dir: state_dir.join("rqbit-session"),
            aria2_session_file: state_dir.join("aria2.session"),
            retry_state_file: state_dir.join("retry-state.json"),
            queue_state_file: state_dir.join("queue-state.json"),
            user_service_dir: config_dir.join("systemd/user"),
            user_service_file: config_dir.join("systemd/user/ariatui-daemon.service"),
            system_service_file: root.join("ariatui-daemon.service"),
        };
        let app = Arc::new(AppContext::new(
            paths.clone(),
            AppConfig::default(),
            crate::state::PersistedState::default(),
            "/tmp/ariatui".into(),
            "test-build".into(),
        ));
        let daemon = DaemonState::new(app).await.unwrap();

        daemon
            .set_download_batch("gid-a", Some(4))
            .await
            .expect("batch saved");
        let reloaded = queue::QueueState::load(&paths.queue_state_file);
        assert_eq!(reloaded.batch("gid-a"), Some(4));

        daemon
            .set_download_batch("gid-a", None)
            .await
            .expect("batch cleared");
        let reloaded = queue::QueueState::load(&paths.queue_state_file);
        assert_eq!(reloaded.batch("gid-a"), None);

        assert!(
            daemon
                .set_download_batch("gid-b", Some(crate::state::MAX_QUEUE_BATCH + 1))
                .await
                .is_err(),
            "batch numbers are bounded"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retry_preserves_http_output_location() {
        let entry = RetryEntry {
            source_uri: "https://example.com/file".into(),
            output_path: Some("/downloads/release.iso".into()),
            current_gid: "old".into(),
            retries: 1,
            next_attempt_at: None,
            daily: false,
            batch: None,
        };
        assert_eq!(
            retry_options(&entry),
            json!({"dir": "/downloads", "out": "release.iso"})
        );
    }

    #[test]
    fn completed_retry_clears_state_when_aria2_changed_the_gid() {
        let source_uri = "https://example.com/release.iso";
        let output_path = "/downloads/release.iso";
        let key = retry_key(source_uri, Some(output_path));
        let mut state = RetryState::default();
        state.entries.insert(
            key,
            RetryEntry {
                source_uri: source_uri.into(),
                output_path: Some(output_path.into()),
                current_gid: "failed-attempt".into(),
                retries: 2,
                next_attempt_at: None,
                daily: false,
                batch: None,
            },
        );

        let changed = remove_completed_retry_entries(
            &mut state,
            &[history_item("successful-retry", DownloadStatus::Complete)],
        );

        assert!(changed);
        assert!(state.entries.is_empty());
    }
}
