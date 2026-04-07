use std::fmt;

use color_eyre::eyre::{Result, bail};

pub fn parse_limit(input: &str) -> Result<Option<u64>> {
    let value = input.trim();
    if value.eq_ignore_ascii_case("unlimited") {
        return Ok(None);
    }
    if value.is_empty() {
        bail!("speed limit cannot be empty");
    }

    let split_at = value
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(value.len());
    let (digits, suffix) = value.split_at(split_at);
    if digits.is_empty() {
        bail!("speed limit must start with digits or 'unlimited'");
    }
    let base: f64 = digits.parse()?;
    if !base.is_finite() || base < 0.0 {
        bail!("speed limit must be a positive number");
    }
    let normalized = normalize_limit_suffix(suffix);
    let multiplier = match normalized.as_str() {
        "" | "B" | "BYTE" | "BYTES" => 1_f64,
        "K" | "KB" | "KIB" | "KBYTE" | "KBYTES" | "KBPS" | "KIBPS" => 1024_f64,
        "M" | "MB" | "MIB" | "MBPS" | "MIBPS" | "MPBS" => 1024_f64 * 1024_f64,
        "G" | "GB" | "GIB" | "GBPS" | "GIBPS" => 1024_f64 * 1024_f64 * 1024_f64,
        other => bail!("unsupported speed suffix '{other}'"),
    };
    let bytes_per_sec = (base * multiplier).round();
    if bytes_per_sec > u64::MAX as f64 {
        bail!("speed limit is too large");
    }
    Ok(Some(bytes_per_sec as u64))
}

fn normalize_limit_suffix(suffix: &str) -> String {
    suffix
        .trim()
        .to_ascii_uppercase()
        .replace([' ', '-', '_', '.'], "")
        .replace("/S", "")
        .replace("PS", "")
}

pub fn format_limit(limit: Option<u64>) -> String {
    match limit {
        None => "unlimited".to_string(),
        Some(value) if value >= 1024 * 1024 * 1024 && value % (1024 * 1024 * 1024) == 0 => {
            format!("{}G", value / (1024 * 1024 * 1024))
        }
        Some(value) if value >= 1024 * 1024 && value % (1024 * 1024) == 0 => {
            format!("{}M", value / (1024 * 1024))
        }
        Some(value) if value >= 1024 && value % 1024 == 0 => format!("{}K", value / 1024),
        Some(value) => value.to_string(),
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut idx = 0;
    let mut value = bytes as f64;
    while value >= 1024.0 && idx < UNITS.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{bytes} {}", UNITS[idx])
    } else {
        format!("{value:.1} {}", UNITS[idx])
    }
}

pub fn format_bytes_per_sec(bytes: u64) -> String {
    format!("{}/s", format_bytes(bytes))
}

pub fn describe_limit_input(input: &str) -> Result<String> {
    match parse_limit(input)? {
        None => Ok("Unlimited".into()),
        Some(value) => Ok(format!("Applies as {}", format_bytes_per_sec(value))),
    }
}

pub fn format_eta(seconds: Option<u64>) -> String {
    let Some(seconds) = seconds else {
        return "--".into();
    };
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

pub struct Percentage(pub f64);

impl fmt::Display for Percentage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.0}%", self.0 * 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_limits() {
        assert_eq!(parse_limit("unlimited").expect("parse"), None);
        assert_eq!(parse_limit("256K").expect("parse"), Some(256 * 1024));
        assert_eq!(parse_limit("10m").expect("parse"), Some(10 * 1024 * 1024));
        assert_eq!(
            parse_limit("10 mbps").expect("parse"),
            Some(10 * 1024 * 1024)
        );
        assert_eq!(
            parse_limit("10mb/s").expect("parse"),
            Some(10 * 1024 * 1024)
        );
        assert_eq!(
            parse_limit("10mpbs").expect("parse"),
            Some(10 * 1024 * 1024)
        );
        assert_eq!(parse_limit("1 kb/s").expect("parse"), Some(1024));
        assert_eq!(parse_limit("1 kbps").expect("parse"), Some(1024));
        assert_eq!(parse_limit("1.5M").expect("parse"), Some(1572864));
        assert_eq!(parse_limit("0.5 mb/s").expect("parse"), Some(524288));
        assert_eq!(parse_limit("2.25 kbps").expect("parse"), Some(2304));
    }

    #[test]
    fn rejects_bad_limits() {
        assert!(parse_limit("").is_err());
        assert!(parse_limit("fast").is_err());
        assert!(parse_limit("10TB").is_err());
        assert!(parse_limit("1.2.3M").is_err());
    }
}
