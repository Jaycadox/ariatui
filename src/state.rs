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

pub const MIN_QUEUE_SLOTS: u8 = 1;
pub const MAX_QUEUE_SLOTS: u8 = 16;
pub const MAX_QUEUE_BATCH: u32 = 9_999;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TorrentStreamingMode {
    #[default]
    Off,
    StartFirst,
    StartAndEndFirst,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PersistedState {
    pub mode: ManualOrScheduled,
    pub manual_limit: String,
    pub usual_internet_speed: String,
    pub remembered_cancel_behavior: CancelBehaviorPreference,
    pub schedule: Vec<String>,
    pub queue_slots: u8,
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
    pub torrent_streaming_mode: TorrentStreamingMode,
    pub torrent_head_size_mib: u32,
    pub torrent_tail_size_mib: u32,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            mode: ManualOrScheduled::Manual,
            manual_limit: "unlimited".into(),
            usual_internet_speed: "unlimited".into(),
            remembered_cancel_behavior: CancelBehaviorPreference::Ask,
            schedule: vec!["unlimited".into(); 24],
            queue_slots: DEFAULT_QUEUE_SLOTS,
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
            torrent_streaming_mode: TorrentStreamingMode::Off,
            torrent_head_size_mib: 32,
            torrent_tail_size_mib: 4,
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
        let mut state: Self = toml::from_str(&contents).wrap_err("failed to parse state.toml")?;
        // An out-of-range slot count (an older build, a hand-edited file) should
        // not stop the app from starting, so repair it instead of bailing.
        let repaired = state.queue_slots.clamp(MIN_QUEUE_SLOTS, MAX_QUEUE_SLOTS);
        if repaired != state.queue_slots {
            state.queue_slots = repaired;
            state.save(&paths.state_file)?;
        }
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

    pub fn torrent_prioritize_piece_value(&self) -> Result<Option<String>> {
        validate_torrent_size_mib(self.torrent_head_size_mib, "torrent head size")?;
        validate_torrent_size_mib(self.torrent_tail_size_mib, "torrent tail size")?;
        Ok(match self.torrent_streaming_mode {
            TorrentStreamingMode::Off => None,
            TorrentStreamingMode::StartFirst => {
                Some(format!("head={}M", self.torrent_head_size_mib))
            }
            TorrentStreamingMode::StartAndEndFirst => Some(format!(
                "head={}M,tail={}M",
                self.torrent_head_size_mib, self.torrent_tail_size_mib
            )),
        })
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
        validate_queue_slots(self.queue_slots)?;
        validate_bind_address(&self.web_ui_bind_address)?;
        validate_cookie_days(self.web_ui_cookie_days)?;
        if self.web_ui_port == 0 {
            bail!("web ui port must be between 1 and 65535");
        }
        self.torrent_prioritize_piece_value()?;
        Ok(())
    }
}

pub const DEFAULT_QUEUE_SLOTS: u8 = 3;

pub fn validate_queue_slots(value: u8) -> Result<u8> {
    if !(MIN_QUEUE_SLOTS..=MAX_QUEUE_SLOTS).contains(&value) {
        bail!("download slots must be between {MIN_QUEUE_SLOTS} and {MAX_QUEUE_SLOTS}");
    }
    Ok(value)
}

/// Parses a user-entered batch number. `None` means "unassigned" (no batch),
/// while an outer `None` means the input was not a valid batch number.
pub fn parse_queue_batch_token(value: &str) -> Option<Option<u32>> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("unassigned") {
        return Some(None);
    }
    let number = value.parse::<u32>().ok()?;
    validate_queue_batch(Some(number)).ok()
}

pub fn validate_queue_batch(value: Option<u32>) -> Result<Option<u32>> {
    if let Some(number) = value
        && number > MAX_QUEUE_BATCH
    {
        bail!("batch number must be between 0 and {MAX_QUEUE_BATCH}");
    }
    Ok(value)
}

pub fn validate_torrent_size_mib(value: u32, label: &str) -> Result<()> {
    if !(1..=8192).contains(&value) {
        bail!("{label} must be between 1 and 8192 MiB");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_valid() {
        PersistedState::default().validate().expect("valid");
    }

    #[test]
    fn queue_slots_bounds_are_enforced() {
        validate_queue_slots(DEFAULT_QUEUE_SLOTS).expect("valid");
        assert!(validate_queue_slots(0).is_err(), "zero slots is invalid");
        assert!(
            validate_queue_slots(MAX_QUEUE_SLOTS + 1).is_err(),
            "too many slots is invalid"
        );
    }

    #[test]
    fn queue_batch_bounds_are_enforced() {
        assert_eq!(validate_queue_batch(None).unwrap(), None);
        assert_eq!(validate_queue_batch(Some(0)).unwrap(), Some(0));
        assert!(
            validate_queue_batch(Some(MAX_QUEUE_BATCH + 1)).is_err(),
            "batch number is too large"
        );
    }

    #[test]
    fn torrent_priority_option_formats() {
        let mut state = PersistedState {
            torrent_streaming_mode: TorrentStreamingMode::StartFirst,
            ..PersistedState::default()
        };
        assert_eq!(
            state.torrent_prioritize_piece_value().unwrap(),
            Some("head=32M".into())
        );
        state.torrent_streaming_mode = TorrentStreamingMode::StartAndEndFirst;
        assert_eq!(
            state.torrent_prioritize_piece_value().unwrap(),
            Some("head=32M,tail=4M".into())
        );
    }
}
