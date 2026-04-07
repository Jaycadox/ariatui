use color_eyre::eyre::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebhookPingMode {
    #[default]
    None,
    Everyone,
    SpecificId,
}

pub fn validate_discord_webhook_url(input: &str) -> Result<()> {
    let value = input.trim();
    if value.is_empty() {
        return Ok(());
    }
    let url = reqwest::Url::parse(value)?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("webhook URL must use http or https");
    }
    let host = url.host_str().unwrap_or_default();
    if !host.contains("discord.com") && !host.contains("discordapp.com") {
        bail!("webhook URL must point to Discord");
    }
    if !value.contains("/api/webhooks/") {
        bail!("webhook URL does not look like a Discord webhook");
    }
    Ok(())
}

pub fn validate_ping_id(mode: WebhookPingMode, ping_id: Option<&str>) -> Result<Option<String>> {
    match mode {
        WebhookPingMode::None | WebhookPingMode::Everyone => Ok(None),
        WebhookPingMode::SpecificId => {
            let ping_id = ping_id.unwrap_or_default().trim();
            if ping_id.is_empty() {
                bail!("ping id cannot be empty in specific-id mode");
            }
            if !ping_id.chars().all(|c| c.is_ascii_digit()) {
                bail!("ping id must be numeric");
            }
            Ok(Some(ping_id.to_string()))
        }
    }
}

pub fn webhook_enabled(url: &str) -> bool {
    !url.trim().is_empty()
}

pub fn mention_prefix(mode: WebhookPingMode, ping_id: Option<&str>) -> String {
    match mode {
        WebhookPingMode::None => String::new(),
        WebhookPingMode::Everyone => "@everyone ".into(),
        WebhookPingMode::SpecificId => ping_id
            .filter(|id| !id.trim().is_empty())
            .map(|id| format!("<@{id}> <@&{id}> "))
            .unwrap_or_default(),
    }
}

