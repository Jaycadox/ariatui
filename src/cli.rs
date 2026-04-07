use clap::{ArgAction, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "ariatui", version, about = "aria2 download manager TUI")]
pub struct Cli {
    #[arg(short, long, global = true, action = ArgAction::SetTrue)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Commands {
    Ui,
    Daemon,
    Service {
        #[command(subcommand)]
        command: ServiceCommands,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ServiceCommands {
    InstallUser,
    InstallSystem,
    UninstallUser,
    UninstallSystem,
}
