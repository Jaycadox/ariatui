use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::daemon::{DownloadItem, DownloadStatus};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct HistoryFile {
    items: Vec<DownloadItem>,
}

pub fn load(path: &Path) -> Result<Vec<DownloadItem>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents =
        fs::read_to_string(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    let history: HistoryFile = serde_json::from_str(&contents)
        .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
    Ok(deduplicate(history.items))
}

pub fn save(path: &Path, items: &[DownloadItem]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }
    let encoded = serde_json::to_vec_pretty(&HistoryFile {
        items: deduplicate(items.to_vec()),
    })?;
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, encoded)
        .wrap_err_with(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).wrap_err_with(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    Ok(())
}

pub fn merge_terminal_events(
    existing: &[DownloadItem],
    stopped: impl IntoIterator<Item = DownloadItem>,
) -> Vec<DownloadItem> {
    let mut by_gid = existing
        .iter()
        .cloned()
        .map(|item| (item.gid.clone(), item))
        .collect::<HashMap<_, _>>();
    let mut new_gids = Vec::new();

    for item in stopped {
        if !is_persistable_history_item(&item) {
            continue;
        }
        if !by_gid.contains_key(&item.gid) {
            new_gids.push(item.gid.clone());
        }
        by_gid.insert(item.gid.clone(), item);
    }

    let mut merged = Vec::with_capacity(by_gid.len());
    for gid in new_gids.into_iter().rev() {
        if let Some(item) = by_gid.remove(&gid) {
            merged.push(item);
        }
    }
    for item in existing {
        if let Some(item) = by_gid.remove(&item.gid) {
            merged.push(item);
        }
    }
    merged.extend(by_gid.into_values());
    deduplicate(merged)
}

pub fn remove(items: &mut Vec<DownloadItem>, gid: &str) -> bool {
    let old_len = items.len();
    items.retain(|item| item.gid != gid);
    old_len != items.len()
}

pub fn is_persistable_history_item(item: &DownloadItem) -> bool {
    !item.is_metadata_only
        && matches!(
            item.status,
            DownloadStatus::Complete | DownloadStatus::Error | DownloadStatus::Removed
        )
}

fn deduplicate(items: Vec<DownloadItem>) -> Vec<DownloadItem> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.gid.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(gid: &str, status: DownloadStatus, completed_bytes: u64) -> DownloadItem {
        DownloadItem {
            gid: gid.into(),
            status,
            name: format!("{gid}.iso"),
            primary_path: Some(format!("/tmp/{gid}.iso")),
            source_uri: Some(format!("https://example.com/{gid}.iso")),
            info_hash: None,
            num_seeders: None,
            followed_by: Vec::new(),
            belongs_to: None,
            is_metadata_only: false,
            total_bytes: 100,
            completed_bytes,
            download_speed_bps: 0,
            realtime_download_speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: None,
            error_code: None,
            error_message: None,
        }
    }

    #[test]
    fn merge_keeps_existing_when_aria2_drops_old_results() {
        let existing = vec![item("old", DownloadStatus::Complete, 100)];
        let merged = merge_terminal_events(&existing, Vec::new());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].gid, "old");
    }

    #[test]
    fn merge_updates_existing_terminal_item() {
        let existing = vec![item("gid", DownloadStatus::Error, 25)];
        let merged =
            merge_terminal_events(&existing, vec![item("gid", DownloadStatus::Complete, 100)]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].status, DownloadStatus::Complete);
        assert_eq!(merged[0].completed_bytes, 100);
    }

    #[test]
    fn merge_ignores_non_terminal_items() {
        let merged = merge_terminal_events(&[], vec![item("active", DownloadStatus::Active, 10)]);
        assert!(merged.is_empty());
    }
}
