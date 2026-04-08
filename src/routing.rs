use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadRoutingRule {
    pub pattern: String,
    pub directory: String,
}

#[derive(Debug, Clone)]
pub struct MatchedRoute {
    pub index: usize,
    pub rule: DownloadRoutingRule,
    pub resolved_directory: PathBuf,
}

pub fn normalize_rules(
    default_download_dir: &str,
    rules: &[DownloadRoutingRule],
) -> Vec<DownloadRoutingRule> {
    let mut normalized = rules
        .iter()
        .filter(|rule| rule.pattern.trim() != "*")
        .cloned()
        .collect::<Vec<_>>();
    normalized.push(DownloadRoutingRule {
        pattern: "*".into(),
        directory: default_download_dir.to_string(),
    });
    normalized
}

pub fn validate_rules(default_download_dir: &str, rules: &[DownloadRoutingRule]) -> Result<()> {
    validate_directory_input(default_download_dir)?;
    let normalized = normalize_rules(default_download_dir, rules);
    for (index, rule) in normalized.iter().enumerate() {
        validate_rule(rule, index == normalized.len() - 1)?;
    }
    Ok(())
}

pub fn validate_rule(rule: &DownloadRoutingRule, is_fallback: bool) -> Result<()> {
    let pattern = rule.pattern.trim();
    if pattern.is_empty() {
        bail!("pattern cannot be empty");
    }
    if is_fallback {
        if pattern != "*" {
            bail!("fallback rule must use '*'");
        }
    } else {
        if pattern == "*" {
            bail!("'*' is reserved for the fallback rule");
        }
        Regex::new(pattern)?;
    }
    validate_directory_input(&rule.directory)?;
    Ok(())
}

pub fn validate_directory_input(input: &str) -> Result<PathBuf> {
    let path = expand_home(input);
    if input.trim().is_empty() {
        bail!("directory cannot be empty");
    }
    if path.exists() {
        if path.is_dir() {
            return Ok(path);
        }
        bail!("path exists but is not a directory");
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("/"));
    if parent.exists() && parent.is_dir() {
        return Ok(path);
    }
    bail!("parent directory does not exist");
}

pub fn match_rule(
    default_download_dir: &str,
    rules: &[DownloadRoutingRule],
    filename: &str,
) -> Result<MatchedRoute> {
    let normalized = normalize_rules(default_download_dir, rules);
    for (index, rule) in normalized.into_iter().enumerate() {
        let is_match = if rule.pattern == "*" {
            true
        } else {
            Regex::new(&rule.pattern)?.is_match(filename)
        };
        if is_match {
            let resolved_directory = expand_home(&rule.directory);
            return Ok(MatchedRoute {
                index,
                rule,
                resolved_directory,
            });
        }
    }
    bail!("no matching download rule found")
}

pub fn expand_home(input: &str) -> PathBuf {
    if let Some(stripped) = input.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(stripped);
    }
    PathBuf::from(input)
}

pub fn describe_directory_input(input: &str) -> Result<String> {
    let path = validate_directory_input(input)?;
    if path.exists() {
        Ok(format!("Directory OK: {}", path.display()))
    } else {
        Ok(format!("Directory will be created: {}", path.display()))
    }
}
