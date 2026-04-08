use std::cmp::Ordering;

use crate::daemon::{DownloadItem, DownloadStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentFilter {
    All,
    Active,
    Waiting,
    Paused,
}

impl CurrentFilter {
    pub fn cycle(self) -> Self {
        match self {
            Self::All => Self::Active,
            Self::Active => Self::Waiting,
            Self::Waiting => Self::Paused,
            Self::Paused => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Active => "active",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
        }
    }

    pub fn from_query(value: &str) -> Self {
        match value {
            "active" => Self::Active,
            "waiting" => Self::Waiting,
            "paused" => Self::Paused,
            _ => Self::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentSort {
    Queue,
    Name,
    Progress,
    Speed,
    Eta,
}

impl CurrentSort {
    pub fn cycle(self) -> Self {
        match self {
            Self::Queue => Self::Name,
            Self::Name => Self::Progress,
            Self::Progress => Self::Speed,
            Self::Speed => Self::Eta,
            Self::Eta => Self::Queue,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Name => "name",
            Self::Progress => "progress",
            Self::Speed => "speed",
            Self::Eta => "eta",
        }
    }

    pub fn from_query(value: &str) -> Self {
        match value {
            "name" => Self::Name,
            "progress" => Self::Progress,
            "speed" => Self::Speed,
            "eta" => Self::Eta,
            _ => Self::Queue,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryFilter {
    All,
    Complete,
    Error,
    Removed,
}

impl HistoryFilter {
    pub fn cycle(self) -> Self {
        match self {
            Self::All => Self::Complete,
            Self::Complete => Self::Error,
            Self::Error => Self::Removed,
            Self::Removed => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Complete => "complete",
            Self::Error => "error",
            Self::Removed => "removed",
        }
    }

    pub fn from_query(value: &str) -> Self {
        match value {
            "complete" => Self::Complete,
            "error" => Self::Error,
            "removed" => Self::Removed,
            _ => Self::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistorySort {
    Recent,
    Name,
    Size,
    Status,
}

impl HistorySort {
    pub fn cycle(self) -> Self {
        match self {
            Self::Recent => Self::Name,
            Self::Name => Self::Size,
            Self::Size => Self::Status,
            Self::Status => Self::Recent,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Name => "name",
            Self::Size => "size",
            Self::Status => "status",
        }
    }

    pub fn from_query(value: &str) -> Self {
        match value {
            "name" => Self::Name,
            "size" => Self::Size,
            "status" => Self::Status,
            _ => Self::Recent,
        }
    }
}

pub fn current_visible_items<'a>(
    items: &'a [DownloadItem],
    search: &str,
    filter: CurrentFilter,
    sort: CurrentSort,
) -> Vec<&'a DownloadItem> {
    let mut visible = items
        .iter()
        .filter(|item| matches_current_filter(item, filter))
        .filter(|item| matches_search(item, search))
        .collect::<Vec<_>>();
    sort_current(&mut visible, sort);
    visible
}

pub fn history_visible_items<'a>(
    items: &'a [DownloadItem],
    search: &str,
    filter: HistoryFilter,
    sort: HistorySort,
) -> Vec<&'a DownloadItem> {
    let mut visible = items
        .iter()
        .filter(|item| matches_history_filter(item, filter))
        .filter(|item| matches_search(item, search))
        .collect::<Vec<_>>();
    sort_history(&mut visible, sort);
    visible
}

fn matches_current_filter(item: &DownloadItem, filter: CurrentFilter) -> bool {
    match filter {
        CurrentFilter::All => true,
        CurrentFilter::Active => item.status == DownloadStatus::Active,
        CurrentFilter::Waiting => item.status == DownloadStatus::Waiting,
        CurrentFilter::Paused => item.status == DownloadStatus::Paused,
    }
}

fn matches_history_filter(item: &DownloadItem, filter: HistoryFilter) -> bool {
    match filter {
        HistoryFilter::All => true,
        HistoryFilter::Complete => item.status == DownloadStatus::Complete,
        HistoryFilter::Error => item.status == DownloadStatus::Error,
        HistoryFilter::Removed => item.status == DownloadStatus::Removed,
    }
}

fn matches_search(item: &DownloadItem, search: &str) -> bool {
    let needle = search.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return true;
    }
    [
        item.name.as_str(),
        item.gid.as_str(),
        item.primary_path.as_deref().unwrap_or_default(),
        item.source_uri.as_deref().unwrap_or_default(),
        item.info_hash.as_deref().unwrap_or_default(),
        item.error_message.as_deref().unwrap_or_default(),
    ]
    .iter()
    .any(|value| value.to_ascii_lowercase().contains(&needle))
}

fn sort_current(items: &mut Vec<&DownloadItem>, sort: CurrentSort) {
    match sort {
        CurrentSort::Queue => {}
        CurrentSort::Name => {
            items.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        }
        CurrentSort::Progress => items.sort_by(|left, right| {
            cmp_u64_desc(left.completed_bytes, right.completed_bytes)
                .then(cmp_u64_desc(left.total_bytes, right.total_bytes))
        }),
        CurrentSort::Speed => items
            .sort_by(|left, right| cmp_u64_desc(left.download_speed_bps, right.download_speed_bps)),
        CurrentSort::Eta => {
            items.sort_by(|left, right| cmp_optional_u64_asc(left.eta_seconds, right.eta_seconds))
        }
    }
}

fn sort_history(items: &mut Vec<&DownloadItem>, sort: HistorySort) {
    match sort {
        HistorySort::Recent => {}
        HistorySort::Name => {
            items.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        }
        HistorySort::Size => {
            items.sort_by(|left, right| cmp_u64_desc(left.total_bytes, right.total_bytes))
        }
        HistorySort::Status => {
            items.sort_by(|left, right| status_rank(&left.status).cmp(&status_rank(&right.status)))
        }
    }
}

fn cmp_u64_desc(left: u64, right: u64) -> Ordering {
    right.cmp(&left)
}

fn cmp_optional_u64_asc(left: Option<u64>, right: Option<u64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn status_rank(status: &DownloadStatus) -> u8 {
    match status {
        DownloadStatus::Complete => 0,
        DownloadStatus::Error => 1,
        DownloadStatus::Removed => 2,
        DownloadStatus::Paused => 3,
        DownloadStatus::Waiting => 4,
        DownloadStatus::Active => 5,
        DownloadStatus::Unknown => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, status: DownloadStatus, total_bytes: u64) -> DownloadItem {
        DownloadItem {
            gid: format!("gid-{name}"),
            status,
            name: name.to_string(),
            primary_path: Some(format!("/tmp/{name}")),
            source_uri: Some(format!("https://example.com/{name}")),
            info_hash: None,
            num_seeders: None,
            followed_by: Vec::new(),
            belongs_to: None,
            is_metadata_only: false,
            total_bytes,
            completed_bytes: total_bytes / 2,
            download_speed_bps: total_bytes,
            realtime_download_speed_bps: total_bytes,
            upload_speed_bps: 0,
            eta_seconds: Some(10),
            connections: Some(4),
            error_code: None,
            error_message: None,
        }
    }

    #[test]
    fn current_search_and_filter_work_together() {
        let items = vec![
            item("alpha.iso", DownloadStatus::Active, 100),
            item("beta.iso", DownloadStatus::Paused, 200),
            item("gamma.iso", DownloadStatus::Waiting, 300),
        ];
        let visible =
            current_visible_items(&items, "beta", CurrentFilter::Paused, CurrentSort::Queue);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].name, "beta.iso");
    }

    #[test]
    fn history_sort_by_size_descending() {
        let items = vec![
            item("small.iso", DownloadStatus::Complete, 100),
            item("large.iso", DownloadStatus::Complete, 500),
        ];
        let visible = history_visible_items(&items, "", HistoryFilter::All, HistorySort::Size);
        assert_eq!(visible[0].name, "large.iso");
        assert_eq!(visible[1].name, "small.iso");
    }
}
