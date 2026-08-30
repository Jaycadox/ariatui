use serde::{Deserialize, Serialize};

use crate::{
    routing::DownloadRoutingRule,
    state::{CancelBehaviorPreference, ManualOrScheduled, TorrentStreamingMode},
    webhook::WebhookPingMode,
};

/// Identifies one download batch. `Number` batches run in ascending order and
/// every download sharing a number belongs to the same batch. `Unassigned`
/// downloads form a single final batch that runs after all numbered batches.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
#[serde(untagged)]
pub enum QueueBatchTarget {
    Number(u32),
    #[default]
    Unassigned,
}

impl QueueBatchTarget {
    pub fn of(batch: Option<u32>) -> Self {
        match batch {
            Some(number) => Self::Number(number),
            None => Self::Unassigned,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Number(number) => number.to_string(),
            Self::Unassigned => "unassigned".into(),
        }
    }

    pub fn batch(self) -> Option<u32> {
        match self {
            Self::Number(number) => Some(number),
            Self::Unassigned => None,
        }
    }

    /// Parses `"7"`, `"unassigned"`, or `""` (unassigned) into a batch target.
    pub fn from_token(token: &str) -> Option<Self> {
        crate::state::parse_queue_batch_token(token).map(Self::of)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QueueBatchSummary {
    pub target: QueueBatchTarget,
    pub running: usize,
    pub waiting: usize,
    pub paused: usize,
    pub held: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueSnapshot {
    pub slots: u8,
    pub active_batch: Option<QueueBatchTarget>,
    pub batches: Vec<QueueBatchSummary>,
    pub held_count: usize,
    pub pending_count: usize,
}

impl Default for QueueSnapshot {
    fn default() -> Self {
        Self {
            slots: crate::state::DEFAULT_QUEUE_SLOTS,
            active_batch: None,
            batches: Vec::new(),
            held_count: 0,
            pending_count: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildLifecycle {
    Starting,
    Ready,
    Restarting,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Snapshot {
    pub daemon_status: DaemonStatus,
    pub aria2_status: Aria2ChildStatus,
    pub scheduler: SchedulerSnapshot,
    pub torrents: TorrentSettingsSnapshot,
    pub routing: RoutingSnapshot,
    pub webhooks: WebhookSnapshot,
    pub web_ui: WebUiSnapshot,
    pub global: GlobalStats,
    pub queue: QueueSnapshot,
    pub current_downloads: Vec<DownloadItem>,
    pub history_downloads: Vec<DownloadItem>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonStatus {
    pub socket_path: String,
    pub state_path: String,
    pub config_path: String,
    pub executable_path: String,
    #[serde(alias = "executable_hash")]
    pub build_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Aria2ChildStatus {
    pub lifecycle: ChildLifecycle,
    pub pid: Option<u32>,
    pub rpc_port: Option<u16>,
    pub restart_count: u32,
    pub last_exit: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerSnapshot {
    pub mode: ManualOrScheduled,
    pub manual_limit_bps: Option<u64>,
    pub usual_internet_speed_bps: Option<u64>,
    pub schedule_limits_bps: [Option<u64>; 24],
    pub effective_limit_bps: Option<u64>,
    pub current_hour: u8,
    pub next_change_at_local: String,
    pub remembered_cancel_behavior: CancelBehaviorPreference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingSnapshot {
    pub default_download_dir: String,
    pub rules: Vec<DownloadRoutingRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TorrentSettingsSnapshot {
    pub mode: TorrentStreamingMode,
    pub head_size_mib: u32,
    pub tail_size_mib: u32,
    pub aria2_prioritize_piece: Option<String>,
    pub engine: String,
    pub downloads: Vec<TorrentDownloadSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TorrentDownloadSnapshot {
    pub gid: String,
    pub id: usize,
    pub name: String,
    pub info_hash: String,
    pub output_folder: String,
    pub status: DownloadStatus,
    pub total_bytes: u64,
    pub completed_bytes: u64,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub live_peers: usize,
    pub seen_peers: usize,
    pub queued_peers: usize,
    pub peer_ips: Vec<String>,
    pub piece_map: String,
    pub sequential_download: bool,
    pub files: Vec<TorrentFileSnapshot>,
}

impl Default for TorrentDownloadSnapshot {
    fn default() -> Self {
        Self {
            gid: String::new(),
            id: 0,
            name: String::new(),
            info_hash: String::new(),
            output_folder: String::new(),
            status: DownloadStatus::Unknown,
            total_bytes: 0,
            completed_bytes: 0,
            download_speed_bps: 0,
            upload_speed_bps: 0,
            live_peers: 0,
            seen_peers: 0,
            queued_peers: 0,
            peer_ips: Vec::new(),
            piece_map: String::new(),
            sequential_download: false,
            files: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct TorrentFileSnapshot {
    pub name: String,
    pub length: u64,
    pub completed_bytes: u64,
    pub included: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookSnapshot {
    pub discord_webhook_url: String,
    pub enabled: bool,
    pub ping_mode: WebhookPingMode,
    pub ping_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebUiStatus {
    #[default]
    Disabled,
    Starting,
    Listening,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebUiSnapshot {
    pub enabled: bool,
    pub bind_address: String,
    pub port: u16,
    pub cookie_days: u32,
    pub status: WebUiStatus,
    pub url: String,
    pub auth_configured: bool,
    pub pending_pair_pins: Vec<String>,
    pub active_session_count: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalStats {
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub num_active: u64,
    pub num_waiting: u64,
    pub num_stopped: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadStatus {
    Active,
    Waiting,
    Paused,
    Complete,
    Error,
    Removed,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadItem {
    pub gid: String,
    pub status: DownloadStatus,
    pub name: String,
    pub primary_path: Option<String>,
    pub source_uri: Option<String>,
    pub info_hash: Option<String>,
    pub num_seeders: Option<u32>,
    pub followed_by: Vec<String>,
    pub belongs_to: Option<String>,
    pub is_metadata_only: bool,
    pub total_bytes: u64,
    pub completed_bytes: u64,
    pub download_speed_bps: u64,
    #[serde(default)]
    pub realtime_download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub eta_seconds: Option<u64>,
    pub connections: Option<u32>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    #[serde(default)]
    pub batch: Option<u32>,
    #[serde(default)]
    pub queue_held: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ResolvedHttpUrl {
    pub url: String,
    pub url_filename: String,
    pub remote_filename: Option<String>,
    pub redirect_filename: Option<String>,
    pub final_url: Option<String>,
    pub is_torrent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ApiPayload {
    ResolvedHttpUrl(ResolvedHttpUrl),
    Download {
        download: DownloadItem,
        created: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum ApiRequest {
    Ping,
    GetSnapshot,
    ResolveHttpUrl {
        url: String,
    },
    AddHttpUrl {
        url: String,
        #[serde(default)]
        filename: Option<String>,
        #[serde(default)]
        batch: Option<u32>,
    },
    AddDownload {
        url: String,
        #[serde(default)]
        filename: Option<String>,
        #[serde(default)]
        directory: Option<String>,
        #[serde(default)]
        options: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        idempotency_key: Option<String>,
    },
    Pause {
        gid: String,
        force: bool,
    },
    Resume {
        gid: String,
    },
    Cancel {
        gid: String,
        delete_files: bool,
    },
    RemoveHistory {
        gid: String,
    },
    ChangePosition {
        gid: String,
        offset: i32,
    },
    SetDownloadBatch {
        gid: String,
        batch: Option<u32>,
    },
    HoldQueueBatch {
        target: QueueBatchTarget,
    },
    StartQueueBatch {
        target: QueueBatchTarget,
    },
    SetQueueSlots {
        slots: u8,
    },
    PauseAll,
    ResumeAll,
    PurgeHistory,
    SetMode {
        mode: ManualOrScheduled,
    },
    SetManualLimit {
        limit_bps: Option<u64>,
    },
    SetUsualInternetSpeed {
        limit_bps: Option<u64>,
    },
    SetSchedule {
        limits_bps: Vec<Option<u64>>,
    },
    SetDownloadRouting {
        default_download_dir: String,
        rules: Vec<DownloadRoutingRule>,
    },
    SetTorrentStreamingSettings {
        mode: TorrentStreamingMode,
        head_size_mib: u32,
        tail_size_mib: u32,
    },
    SetWebhookSettings {
        discord_webhook_url: String,
        ping_mode: WebhookPingMode,
        ping_id: Option<String>,
    },
    TriggerWebhookTest,
    SetWebUiSettings {
        enabled: bool,
        bind_address: String,
        port: u16,
        cookie_days: u32,
    },
    ApproveWebUiPin {
        pin: String,
    },
    RevokeAllWebUiSessions,
    SetRememberedCancelBehavior {
        behavior: CancelBehaviorPreference,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiEnvelope {
    pub id: String,
    #[serde(flatten)]
    pub request: ApiRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse {
    pub id: String,
    pub ok: bool,
    pub result: Option<Snapshot>,
    #[serde(default)]
    pub payload: Option<ApiPayload>,
    pub error: Option<ApiError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ApiReply {
    pub snapshot: Snapshot,
    pub payload: Option<ApiPayload>,
}

impl Snapshot {
    pub fn empty(
        socket_path: String,
        state_path: String,
        config_path: String,
        executable_path: String,
        build_id: String,
    ) -> Self {
        Self {
            daemon_status: DaemonStatus {
                socket_path,
                state_path,
                config_path,
                executable_path,
                build_id,
            },
            aria2_status: Aria2ChildStatus {
                lifecycle: ChildLifecycle::Starting,
                pid: None,
                rpc_port: None,
                restart_count: 0,
                last_exit: None,
                last_error: None,
            },
            scheduler: SchedulerSnapshot {
                mode: ManualOrScheduled::Manual,
                manual_limit_bps: None,
                usual_internet_speed_bps: None,
                schedule_limits_bps: [None; 24],
                effective_limit_bps: None,
                current_hour: 0,
                next_change_at_local: "01:00".into(),
                remembered_cancel_behavior: CancelBehaviorPreference::Ask,
            },
            torrents: TorrentSettingsSnapshot::default(),
            routing: RoutingSnapshot {
                default_download_dir: "~/Downloads".into(),
                rules: vec![DownloadRoutingRule {
                    pattern: "*".into(),
                    directory: "~/Downloads".into(),
                }],
            },
            webhooks: WebhookSnapshot::default(),
            web_ui: WebUiSnapshot::default(),
            global: GlobalStats::default(),
            queue: QueueSnapshot::default(),
            current_downloads: Vec::new(),
            history_downloads: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

impl Default for Aria2ChildStatus {
    fn default() -> Self {
        Self {
            lifecycle: ChildLifecycle::Starting,
            pid: None,
            rpc_port: None,
            restart_count: 0,
            last_exit: None,
            last_error: None,
        }
    }
}

impl Default for SchedulerSnapshot {
    fn default() -> Self {
        Self {
            mode: ManualOrScheduled::Manual,
            manual_limit_bps: None,
            usual_internet_speed_bps: None,
            schedule_limits_bps: [None; 24],
            effective_limit_bps: None,
            current_hour: 0,
            next_change_at_local: "01:00".into(),
            remembered_cancel_behavior: CancelBehaviorPreference::Ask,
        }
    }
}

impl Default for RoutingSnapshot {
    fn default() -> Self {
        Self {
            default_download_dir: "~/Downloads".into(),
            rules: vec![DownloadRoutingRule {
                pattern: "*".into(),
                directory: "~/Downloads".into(),
            }],
        }
    }
}

impl Default for WebhookSnapshot {
    fn default() -> Self {
        Self {
            discord_webhook_url: String::new(),
            enabled: false,
            ping_mode: WebhookPingMode::None,
            ping_id: None,
        }
    }
}

impl Default for WebUiSnapshot {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: "0.0.0.0".into(),
            port: 39123,
            cookie_days: 30,
            status: WebUiStatus::Disabled,
            url: "http://127.0.0.1:39123".into(),
            auth_configured: true,
            pending_pair_pins: Vec::new(),
            active_session_count: 0,
            last_error: None,
        }
    }
}
