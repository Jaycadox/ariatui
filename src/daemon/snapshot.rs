use serde::{Deserialize, Serialize};

use crate::{
    routing::DownloadRoutingRule,
    state::{CancelBehaviorPreference, ManualOrScheduled},
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
    pub routing: RoutingSnapshot,
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
    pub total_bytes: u64,
    pub completed_bytes: u64,
    pub download_speed_bps: u64,
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
    SetMode {
        mode: ManualOrScheduled,
    },
    SetManualLimit {
        limit_bps: Option<u64>,
    },
    SetSchedule {
        limits_bps: Vec<Option<u64>>,
    },
    SetDownloadRouting {
        default_download_dir: String,
        rules: Vec<DownloadRoutingRule>,
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
                schedule_limits_bps: [None; 24],
                effective_limit_bps: None,
                current_hour: 0,
                next_change_at_local: "01:00".into(),
                remembered_cancel_behavior: CancelBehaviorPreference::Ask,
            },
            routing: RoutingSnapshot {
                default_download_dir: "~/Downloads".into(),
                rules: vec![DownloadRoutingRule {
                    pattern: "*".into(),
                    directory: "~/Downloads".into(),
                }],
            },
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
