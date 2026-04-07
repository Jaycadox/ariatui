use std::{fs, path::Path};

use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::AppPaths;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub daemon: DaemonConfig,
    pub ui: UiConfig,
    pub web: WebConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
#[serde(default)]
pub struct UiConfig {
    pub refresh_interval_ms: u64,
    pub show_details_by_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub default_enabled: bool,
    pub default_bind_address: String,
    pub default_port: u16,
    pub default_cookie_days: u32,
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
            web: WebConfig {
                default_enabled: false,
                default_bind_address: "0.0.0.0".into(),
                default_port: 39123,
                default_cookie_days: 30,
            },
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        AppConfig::default().daemon
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        AppConfig::default().ui
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        AppConfig::default().web
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
