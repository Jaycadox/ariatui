pub mod child;
pub mod marker;
pub mod reconcile;
pub mod server;
pub mod service;
pub mod snapshot;

use std::sync::Arc;

use color_eyre::eyre::Result;
use tokio::sync::RwLock;

use crate::{config::AppConfig, paths::AppPaths, state::PersistedState, web};

pub use self::{
    reconcile::{DaemonState, SharedDaemonState},
    snapshot::{
        ApiEnvelope, ApiError, ApiPayload, ApiRequest, ApiResponse, ChildLifecycle, DownloadItem,
        DownloadStatus, ResolvedHttpUrl, Snapshot,
    },
};

#[derive(Debug)]
pub struct AppContext {
    pub paths: AppPaths,
    pub config: AppConfig,
    pub state: RwLock<PersistedState>,
    pub current_executable_path: String,
    pub current_build_id: String,
}

pub type SharedApp = Arc<AppContext>;

impl AppContext {
    pub fn new(
        paths: AppPaths,
        config: AppConfig,
        state: PersistedState,
        current_executable_path: String,
        current_build_id: String,
    ) -> Self {
        Self {
            paths,
            config,
            state: RwLock::new(state),
            current_executable_path,
            current_build_id,
        }
    }
}

pub async fn run(app: SharedApp) -> Result<()> {
    let daemon_state = DaemonState::new(app.clone()).await?;
    let shared = Arc::new(daemon_state);
    let _marker = marker::DaemonMarker::create(&app)?;

    let reconcile_task = tokio::spawn(reconcile::run(shared.clone()));
    let server_task = tokio::spawn(server::run(shared.clone()));
    let web_task = tokio::spawn(web::supervise(shared.clone()));

    tokio::select! {
        result = reconcile_task => result??,
        result = server_task => result??,
        result = web_task => result??,
    }

    Ok(())
}
