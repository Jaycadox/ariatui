use std::{fs, path::Path, sync::Arc};

use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::daemon::AppContext;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonMarkerInfo {
    pub pid: u32,
    #[serde(alias = "executable_hash")]
    pub build_id: String,
    pub executable_path: String,
}

#[derive(Debug)]
pub struct DaemonMarker {
    path: std::path::PathBuf,
}

impl DaemonMarker {
    pub fn create(app: &Arc<AppContext>) -> Result<Self> {
        let info = DaemonMarkerInfo {
            pid: std::process::id(),
            build_id: app.current_build_id.clone(),
            executable_path: app.current_executable_path.clone(),
        };
        let encoded = serde_json::to_vec(&info)?;
        fs::write(&app.paths.daemon_marker_file, encoded).wrap_err_with(|| {
            format!(
                "failed to write daemon marker {}",
                app.paths.daemon_marker_file.display()
            )
        })?;
        Ok(Self {
            path: app.paths.daemon_marker_file.clone(),
        })
    }
}

impl Drop for DaemonMarker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn read(path: &Path) -> Result<DaemonMarkerInfo> {
    let contents = fs::read(path)
        .wrap_err_with(|| format!("failed to read daemon marker {}", path.display()))?;
    Ok(serde_json::from_slice(&contents)?)
}
