use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "ariatui",
    version,
    about = "Download manager TUI and agent-friendly CLI"
)]
pub struct Cli {
    #[arg(short, long, global = true, action = ArgAction::SetTrue)]
    pub verbose: bool,
    /// Emit machine-readable JSON. Equivalent to --format json.
    #[arg(long, global = true, conflicts_with = "format")]
    pub json: bool,
    /// Output format. Human output is descriptive; JSON is stable for agents.
    #[arg(long, global = true, value_enum)]
    pub format: Option<OutputFormat>,
    /// Override the daemon Unix socket.
    #[arg(long, global = true, env = "ARIATUI_SOCKET")]
    pub socket: Option<PathBuf>,
    /// Daemon request timeout, such as 5s or 2m.
    #[arg(long, global = true, default_value = "10s")]
    pub timeout: String,
    #[arg(long, global = true)]
    pub request_id: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum OutputFormat {
    Human,
    Json,
    Jsonl,
    Tsv,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Commands {
    Ui,
    Daemon,
    Status {
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value = "1s")]
        interval: String,
    },
    Doctor,
    Capabilities,
    /// Stream changed download records. JSONL is recommended for agents.
    Events {
        #[arg(long, default_value = "1s")]
        interval: String,
        /// Stop after this many events; omit to follow indefinitely.
        #[arg(long)]
        count: Option<usize>,
    },
    /// Execute daemon API requests from a JSON array or stdin (-).
    Batch {
        #[arg(long, default_value = "-")]
        file: PathBuf,
        #[arg(long)]
        stop_on_error: bool,
    },
    Api {
        #[command(subcommand)]
        command: ApiCommands,
    },
    Schema {
        command: Option<String>,
    },
    Download {
        #[command(subcommand)]
        command: DownloadCommands,
    },
    Queue {
        #[command(subcommand)]
        command: QueueCommands,
    },
    History {
        #[command(subcommand)]
        command: HistoryCommands,
    },
    Speed {
        #[command(subcommand)]
        command: SpeedCommands,
    },
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommands,
    },
    Route {
        #[command(subcommand)]
        command: RouteCommands,
    },
    Torrent {
        #[command(subcommand)]
        command: TorrentCommands,
    },
    Web {
        #[command(subcommand)]
        command: WebCommands,
    },
    Webhook {
        #[command(subcommand)]
        command: WebhookCommands,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    Service {
        #[command(subcommand)]
        command: ServiceCommands,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ApiCommands {
    /// Send a raw typed request, e.g. '{"method":"get_snapshot"}'.
    Request { payload: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum DownloadCommands {
    Add(AddArgs),
    Resolve {
        url: String,
    },
    List(ListArgs),
    Show(Selector),
    Files(Selector),
    Pause {
        #[command(flatten)]
        selector: Selector,
        #[arg(long)]
        force: bool,
    },
    Resume(Selector),
    Cancel {
        #[command(flatten)]
        selector: Selector,
        #[arg(long)]
        delete_files: bool,
        #[arg(long)]
        yes: bool,
    },
    Wait {
        #[command(flatten)]
        selector: Selector,
        #[arg(long, default_value = "complete,error")]
        until: String,
        #[arg(long, default_value = "1h")]
        wait_timeout: String,
        #[arg(long, default_value = "1s")]
        interval: String,
    },
    Move {
        #[command(flatten)]
        selector: Selector,
        #[arg(long, allow_hyphen_values = true)]
        offset: i32,
    },
    PauseAll,
    ResumeAll,
}

#[derive(Debug, Clone, Args)]
pub struct AddArgs {
    pub url: String,
    #[arg(long)]
    pub dir: Option<String>,
    #[arg(long, alias = "filename")]
    pub output_name: Option<String>,
    #[arg(long = "header")]
    pub headers: Vec<String>,
    #[arg(long)]
    pub referer: Option<String>,
    #[arg(long)]
    pub user_agent: Option<String>,
    #[arg(long)]
    pub checksum: Option<String>,
    #[arg(long)]
    pub connections: Option<u32>,
    #[arg(long)]
    pub split: Option<u32>,
    #[arg(long)]
    pub max_download_limit: Option<String>,
    /// Advanced aria2 option in KEY=VALUE form. Repeatable.
    #[arg(long = "aria2-option")]
    pub aria2_options: Vec<String>,
    #[arg(long)]
    pub paused: bool,
    #[arg(long)]
    pub dry_run: bool,
    /// Prevent duplicate adds when retrying the same agent operation.
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct ListArgs {
    #[arg(long, value_delimiter = ',')]
    pub status: Vec<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub history: bool,
}

#[derive(Debug, Clone, Args)]
#[group(required = true, multiple = false)]
pub struct Selector {
    #[arg(long)]
    pub gid: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum QueueCommands {
    List,
    Move {
        #[command(flatten)]
        selector: Selector,
        #[arg(long, allow_hyphen_values = true)]
        offset: i32,
    },
    Pause,
    Resume,
}

#[derive(Debug, Clone, Subcommand)]
pub enum HistoryCommands {
    List(ListArgs),
    Show(Selector),
    Remove(Selector),
    Purge {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum SpeedCommands {
    Show,
    Set { limit: String },
    Unlimited,
    Mode { mode: ModeArg },
    Usual { limit: String },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ModeArg {
    Manual,
    Scheduled,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ScheduleCommands {
    Show,
    /// Replace all 24 hourly limits with a comma-separated list.
    Set {
        #[arg(value_delimiter = ',', num_args = 24)]
        limits: Vec<String>,
    },
    /// Set an hour range (0-23, end exclusive; a wrapped range is allowed).
    SetRange {
        #[arg(long)]
        from: u8,
        #[arg(long)]
        to: u8,
        #[arg(long)]
        limit: String,
    },
    Clear,
}

#[derive(Debug, Clone, Subcommand)]
pub enum RouteCommands {
    List,
    Test {
        filename: String,
    },
    Add {
        #[arg(long)]
        pattern: String,
        #[arg(long)]
        directory: String,
        #[arg(long)]
        before: Option<usize>,
    },
    Update {
        index: usize,
        #[arg(long)]
        pattern: Option<String>,
        #[arg(long)]
        directory: Option<String>,
    },
    Remove {
        index: usize,
    },
    Move {
        index: usize,
        #[arg(long, allow_hyphen_values = true)]
        offset: i32,
    },
    SetDefault {
        directory: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum TorrentCommands {
    Show(Selector),
    Files(Selector),
    Peers(Selector),
    Streaming {
        #[command(subcommand)]
        command: StreamingCommands,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum StreamingCommands {
    Show,
    Set {
        mode: StreamingModeArg,
        #[arg(long, default_value_t = 32)]
        head_mib: u32,
        #[arg(long, default_value_t = 4)]
        tail_mib: u32,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StreamingModeArg {
    Off,
    StartFirst,
    StartAndEndFirst,
}

#[derive(Debug, Clone, Subcommand)]
pub enum WebCommands {
    Status,
    Enable,
    Disable,
    Configure {
        #[arg(long)]
        bind: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        cookie_days: Option<u32>,
    },
    Pairing {
        #[command(subcommand)]
        command: PairingCommands,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum PairingCommands {
    List,
    Approve { pin: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum SessionCommands {
    List,
    RevokeAll {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum WebhookCommands {
    Show,
    Configure {
        #[arg(long)]
        url: String,
        #[arg(long, value_enum, default_value = "none")]
        ping_mode: PingModeArg,
        #[arg(long)]
        ping_id: Option<String>,
    },
    Disable,
    Test,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PingModeArg {
    None,
    Everyone,
    SpecificId,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCommands {
    Show,
    Validate,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ServiceCommands {
    InstallUser,
    InstallSystem,
    UninstallUser,
    UninstallSystem,
    StartUser,
    StartSystem,
    RestartUser,
    RestartSystem,
    Status,
}
