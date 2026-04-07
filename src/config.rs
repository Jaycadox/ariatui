use std::{fs, path::Path};

use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::AppPaths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub daemon: DaemonConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub poll_interval_ms: u64,
    pub rpc_request_timeout_secs: u64,
    pub socket_path: String,
    pub aria2_bin: String,
    pub stopped_history_limit: usize,
    pub waiting_limit: usize,
    pub download_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub refresh_interval_ms: u64,
    pub show_details_by_default: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig {
                poll_interval_ms: 1000,
                rpc_request_timeout_secs: 5,
                socket_path: String::new(),
                aria2_bin: "aria2c".into(),
                stopped_history_limit: 50,
                waiting_limit: 100,
                download_dir: "~/Downloads".into(),
            },
            ui: UiConfig {
                refresh_interval_ms: 250,
                show_details_by_default: true,
            },
        }
    }
}

impl AppConfig {
    pub fn load_or_create(paths: &AppPaths) -> Result<Self> {
        paths.ensure_dirs()?;
        if !paths.config_file.exists() {
            let config = Self::default();
            config.save(&paths.config_file)?;
            return Ok(config);
        }

        let contents = fs::read_to_string(&paths.config_file)
            .wrap_err_with(|| format!("failed to read {}", paths.config_file.display()))?;
        toml::from_str(&contents).wrap_err("failed to parse config.toml")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let serialized = toml::to_string_pretty(self)?;
        fs::write(path, serialized).wrap_err_with(|| format!("failed to write {}", path.display()))
    }
}
