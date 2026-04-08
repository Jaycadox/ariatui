use serde::{Deserialize, Serialize};

use crate::{
    routing::DownloadRoutingRule,
    state::{CancelBehaviorPreference, ManualOrScheduled, TorrentStreamingMode},
    webhook::WebhookPingMode,
};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
