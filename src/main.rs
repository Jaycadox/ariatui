mod binary_info;
mod bootstrap;
mod cli;
mod config;
mod daemon;
mod download_uri;
mod eta;
mod list_view;
mod paths;
mod routing;
mod rpc;
mod schedule;
mod startup;
mod state;
mod tui;
mod units;
mod web;
mod webhook;

use std::sync::Arc;

use clap::Parser;
use color_eyre::eyre::WrapErr;

use crate::{
    binary_info::{current_build_id, current_executable_path},
    bootstrap::BootstrapAction,
    cli::{Cli, Commands, ServiceCommands},
    config::AppConfig,
    daemon::{AppContext, service},
    paths::AppPaths,
    startup::StartupTrace,
    state::PersistedState,
};

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                if cli.verbose {
                    "ariatui=debug".into()
                } else {
                    "ariatui=info".into()
                }
            }),
        )
        .init();
    let mut startup = StartupTrace::new(cli.verbose);
    startup.mark("startup.begin");

    let paths = AppPaths::discover()?;
    startup.mark("paths.discovered");
    let current_executable = current_executable_path()?;
    startup.mark("current_executable.resolved");
    let current_build_id = current_build_id();
    startup.mark("current_build_id.ready");
    let config = AppConfig::load_or_create(&paths)?;
    startup.mark("config.loaded");
    let state = PersistedState::load_or_create(&paths)?;
    startup.mark("state.loaded");
    let context = Arc::new(AppContext::new(
        paths,
        config,
        state,
        current_executable.display().to_string(),
        current_build_id,
    ));
    startup.mark("app_context.ready");

    if cli.command.is_none() {
        startup.mark("bootstrap.default.begin");
        return match bootstrap::run_default_flow(context.as_ref()).await? {
            BootstrapAction::StartUi { initial_snapshot } => {
                startup.mark("bootstrap.default.done");
                tui::run(context, initial_snapshot).await
            }
            BootstrapAction::Exit => Ok(()),
        };
    }

    startup.mark("cli.command.dispatch");
    match cli.command.unwrap_or(Commands::Ui) {
        Commands::Ui => tui::run(context, None).await,
        Commands::Daemon => daemon::run(context).await,
        Commands::Service { command } => match command {
            ServiceCommands::InstallUser => {
                service::install_user(context.as_ref()).wrap_err("failed to install user service")
            }
            ServiceCommands::InstallSystem => service::install_system(context.as_ref())
                .wrap_err("failed to install system service"),
            ServiceCommands::UninstallUser => service::uninstall_user(context.as_ref())
                .wrap_err("failed to uninstall user service"),
            ServiceCommands::UninstallSystem => service::uninstall_system(context.as_ref())
                .wrap_err("failed to uninstall system service"),
        },
    }
}
