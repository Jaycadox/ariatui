use std::{fs, path::Path};

use color_eyre::eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    paths::AppPaths,
    routing::{DownloadRoutingRule, validate_rules},
    units,
    web::{validate_bind_address, validate_cookie_days},
    webhook::{WebhookPingMode, validate_discord_webhook_url, validate_ping_id},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ManualOrScheduled {
    #[default]
    Manual,
    Scheduled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CancelBehaviorPreference {
    #[default]
    Ask,
    KeepPartials,
    DeletePartials,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PersistedState {
    pub mode: ManualOrScheduled,
    pub manual_limit: String,
    pub usual_internet_speed: String,
    pub remembered_cancel_behavior: CancelBehaviorPreference,
    pub schedule: Vec<String>,
    pub default_download_dir: String,
    pub download_rules: Vec<DownloadRoutingRule>,
    pub discord_webhook_url: String,
    pub webhook_ping_mode: WebhookPingMode,
    pub webhook_ping_id: String,
    pub web_ui_enabled: bool,
    pub web_ui_bind_address: String,
    pub web_ui_port: u16,
    pub web_ui_cookie_days: u32,
    pub web_ui_session_secret: String,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            mode: ManualOrScheduled::Manual,
            manual_limit: "unlimited".into(),
            usual_internet_speed: "unlimited".into(),
            remembered_cancel_behavior: CancelBehaviorPreference::Ask,
            schedule: vec!["unlimited".into(); 24],
            default_download_dir: "~/Downloads".into(),
            download_rules: vec![DownloadRoutingRule {
                pattern: "*".into(),
                directory: "~/Downloads".into(),
            }],
            discord_webhook_url: String::new(),
            webhook_ping_mode: WebhookPingMode::None,
            webhook_ping_id: String::new(),
            web_ui_enabled: false,
            web_ui_bind_address: "0.0.0.0".into(),
            web_ui_port: 39123,
            web_ui_cookie_days: 30,
            web_ui_session_secret: String::new(),
        }
    }
}

impl PersistedState {
    pub fn load_or_create(paths: &AppPaths) -> Result<Self> {
        paths.ensure_dirs()?;
        if !paths.state_file.exists() {
            let state = Self::default();
            state.save(&paths.state_file)?;
            return Ok(state);
        }
        let contents = fs::read_to_string(&paths.state_file)
            .wrap_err_with(|| format!("failed to read {}", paths.state_file.display()))?;
        let state: Self = toml::from_str(&contents).wrap_err("failed to parse state.toml")?;
        state.validate()?;
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let serialized = toml::to_string_pretty(self)?;
        fs::write(path, serialized).wrap_err_with(|| format!("failed to write {}", path.display()))
    }

    pub fn manual_limit_bps(&self) -> Result<Option<u64>> {
        units::parse_limit(&self.manual_limit)
    }

    pub fn usual_internet_speed_bps(&self) -> Result<Option<u64>> {
        units::parse_limit(&self.usual_internet_speed)
    }

    pub fn schedule_bps(&self) -> Result<[Option<u64>; 24]> {
        let parsed: Vec<Option<u64>> = self
            .schedule
            .iter()
            .map(|value| units::parse_limit(value))
            .collect::<Result<Vec<_>>>()?;
        parsed
            .try_into()
            .map_err(|_| color_eyre::eyre::eyre!("schedule must contain 24 entries"))
    }

    pub fn validate(&self) -> Result<()> {
        if self.schedule.len() != 24 {
            bail!("schedule must contain exactly 24 entries");
        }
        self.manual_limit_bps()?;
        self.usual_internet_speed_bps()?;
        self.schedule_bps()?;
        validate_rules(&self.default_download_dir, &self.download_rules)?;
        validate_discord_webhook_url(&self.discord_webhook_url)?;
        let _ = validate_ping_id(self.webhook_ping_mode, Some(&self.webhook_ping_id))?;
        validate_bind_address(&self.web_ui_bind_address)?;
        validate_cookie_days(self.web_ui_cookie_days)?;
        if self.web_ui_port == 0 {
            bail!("web ui port must be between 1 and 65535");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_valid() {
        PersistedState::default().validate().expect("valid");
    }
}
