use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use crate::daemon::{DownloadItem, DownloadStatus};

const MIN_WINDOW: Duration = Duration::from_secs(4);
const DEFAULT_WINDOW: Duration = Duration::from_secs(8);
const MAX_WINDOW: Duration = Duration::from_secs(18);
const RETENTION_WINDOW: Duration = Duration::from_secs(45);

#[derive(Debug, Default)]
pub(crate) struct RollingSpeedTracker {
    histories: HashMap<String, TransferHistory>,
}

#[derive(Debug, Clone, Copy)]
struct TransferSample {
    at: Instant,
    completed_bytes: u64,
}

#[derive(Debug, Default)]
struct TransferHistory {
    samples: VecDeque<TransferSample>,
}

impl RollingSpeedTracker {
    pub(crate) fn refresh(&mut self, now: Instant, items: &mut [DownloadItem]) {
        let mut live_gids = HashSet::new();

        for item in items.iter_mut() {
            item.realtime_download_speed_bps = item.download_speed_bps;

            if item.status != DownloadStatus::Active {
                if item.realtime_download_speed_bps == 0 {
                    item.download_speed_bps = 0;
                    item.eta_seconds = None;
                }
                continue;
            }

            live_gids.insert(item.gid.clone());
            let history = self.histories.entry(item.gid.clone()).or_default();
            history.record(now, item.completed_bytes);

            let rolling_speed_bps =
                history.rolling_speed_bps(item, item.realtime_download_speed_bps);
            item.download_speed_bps = rolling_speed_bps;
            item.eta_seconds = rolling_eta_seconds(item, rolling_speed_bps);
        }

        self.histories
            .retain(|gid, history| live_gids.contains(gid) || history.is_recent(now));
    }
}

impl TransferHistory {
    fn record(&mut self, now: Instant, completed_bytes: u64) {
        if let Some(last) = self.samples.back().copied()
            && completed_bytes < last.completed_bytes
        {
            self.samples.clear();
        }

        self.samples.push_back(TransferSample {
            at: now,
            completed_bytes,
        });
        self.prune(now);
    }

    fn rolling_speed_bps(&self, item: &DownloadItem, realtime_speed_bps: u64) -> u64 {
        let Some(latest) = self.samples.back().copied() else {
            return realtime_speed_bps;
        };

        let window = adaptive_window(item, realtime_speed_bps);
        let stable_since = self
            .samples
            .iter()
            .rev()
            .take_while(|sample| sample.completed_bytes == latest.completed_bytes)
            .last()
            .copied();
        if let Some(stable_since) = stable_since
            && latest.at.duration_since(stable_since.at) >= window
        {
            return 0;
        }
        let earliest = self
            .samples
            .iter()
            .rev()
            .find(|sample| latest.at.duration_since(sample.at) >= window)
            .copied()
            .or_else(|| self.samples.front().copied());

        let Some(earliest) = earliest else {
            return realtime_speed_bps;
        };

        let elapsed = latest.at.duration_since(earliest.at);
        if elapsed.is_zero() {
            return realtime_speed_bps;
        }

        let transferred_bytes = latest
            .completed_bytes
            .saturating_sub(earliest.completed_bytes);
        if transferred_bytes == 0 {
            return if elapsed >= MIN_WINDOW {
                0
            } else {
                realtime_speed_bps
            };
        }

        (transferred_bytes as f64 / elapsed.as_secs_f64())
            .round()
            .max(1.0) as u64
    }

    fn prune(&mut self, now: Instant) {
        while self.samples.len() > 1 {
            let Some(front) = self.samples.front().copied() else {
                break;
            };
            if now.duration_since(front.at) <= RETENTION_WINDOW {
                break;
            }
            self.samples.pop_front();
        }
    }

    fn is_recent(&self, now: Instant) -> bool {
        self.samples
            .back()
            .is_some_and(|sample| now.duration_since(sample.at) <= RETENTION_WINDOW)
    }
}

fn adaptive_window(item: &DownloadItem, realtime_speed_bps: u64) -> Duration {
    let remaining_bytes = item.total_bytes.saturating_sub(item.completed_bytes);
    if remaining_bytes == 0 {
        return MIN_WINDOW;
    }
    if realtime_speed_bps == 0 {
        return DEFAULT_WINDOW;
    }

    // Short downloads need a quicker response, while long downloads can trade
    // a bit of latency for a steadier ETA.
    Duration::from_secs_f64(
        ((remaining_bytes as f64 / realtime_speed_bps as f64) / 6.0)
            .clamp(MIN_WINDOW.as_secs_f64(), MAX_WINDOW.as_secs_f64()),
    )
}

fn rolling_eta_seconds(item: &DownloadItem, speed_bps: u64) -> Option<u64> {
    if speed_bps == 0 || item.total_bytes < item.completed_bytes {
        return None;
    }

    let remaining = item.total_bytes - item.completed_bytes;
    Some(remaining / speed_bps.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_item(
        name: &str,
        total_bytes: u64,
        completed_bytes: u64,
        realtime_speed_bps: u64,
    ) -> DownloadItem {
        DownloadItem {
            gid: format!("gid-{name}"),
            status: DownloadStatus::Active,
            name: name.into(),
            primary_path: None,
            source_uri: None,
            info_hash: None,
            num_seeders: None,
            followed_by: Vec::new(),
            belongs_to: None,
            is_metadata_only: false,
            total_bytes,
            completed_bytes,
            download_speed_bps: realtime_speed_bps,
            realtime_download_speed_bps: realtime_speed_bps,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: None,
            error_code: None,
            error_message: None,
        }
    }

    #[test]
    fn rolling_speed_uses_transferred_bytes_over_time() {
        let base = Instant::now();
        let mut tracker = RollingSpeedTracker::default();
        let mut items = vec![active_item("alpha", 10_000, 0, 2_000)];
        tracker.refresh(base, &mut items);

        items[0].completed_bytes = 1_000;
        items[0].download_speed_bps = 4_000;
        tracker.refresh(base + Duration::from_secs(5), &mut items);

        assert_eq!(items[0].download_speed_bps, 200);
        assert_eq!(items[0].realtime_download_speed_bps, 4_000);
        assert_eq!(items[0].eta_seconds, Some(45));
    }

    #[test]
    fn rolling_speed_drops_to_zero_after_sustained_stall() {
        let base = Instant::now();
        let mut tracker = RollingSpeedTracker::default();
        let mut items = vec![active_item("alpha", 10_000, 0, 1_000)];
        tracker.refresh(base, &mut items);

        items[0].completed_bytes = 2_000;
        items[0].download_speed_bps = 1_000;
        tracker.refresh(base + Duration::from_secs(4), &mut items);

        items[0].download_speed_bps = 0;
        tracker.refresh(base + Duration::from_secs(13), &mut items);

        assert_eq!(items[0].download_speed_bps, 0);
        assert_eq!(items[0].eta_seconds, None);
        assert_eq!(items[0].realtime_download_speed_bps, 0);
    }
}
