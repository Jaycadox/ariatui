use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use crate::daemon::{DownloadItem, DownloadStatus};

// Two time scales let real route/schedule changes through without letting one
// aria2 sample or TCP burst move the UI substantially.
const FAST_TAU_SECONDS: f64 = 5.0;
const SLOW_TAU_SECONDS: f64 = 28.0;
const ETA_IMPROVEMENT_TAU_SECONDS: f64 = 18.0;
const ETA_WORSENING_TAU_SECONDS: f64 = 10.0;
const STALL_ZERO_AFTER: Duration = Duration::from_secs(20);
const SAMPLE_RETENTION: Duration = Duration::from_secs(120);
const HISTORY_RETENTION: Duration = Duration::from_secs(180);
const MAX_RATE_SAMPLES: usize = 45;
const CHANGE_CONFIRMATION_SAMPLES: u8 = 3;

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
    recent_rates: VecDeque<f64>,
    fast_ema: Option<f64>,
    slow_ema: Option<f64>,
    displayed_speed: Option<f64>,
    displayed_eta: Option<f64>,
    last_refresh: Option<Instant>,
    last_progress: Option<Instant>,
    last_seen: Option<Instant>,
    active: bool,
    shock_direction: i8,
    shock_count: u8,
}

impl RollingSpeedTracker {
    pub(crate) fn refresh(&mut self, now: Instant, items: &mut [DownloadItem]) {
        let mut live_gids = HashSet::new();
        for item in items.iter_mut() {
            let realtime_speed = item.download_speed_bps;
            item.realtime_download_speed_bps = realtime_speed;
            live_gids.insert(item.gid.clone());

            let history = self.histories.entry(item.gid.clone()).or_default();
            history.last_seen = Some(now);
            if item.status != DownloadStatus::Active {
                history.mark_inactive();
                item.download_speed_bps = 0;
                item.eta_seconds = None;
                continue;
            }

            let estimate = history.update(now, item.completed_bytes, realtime_speed, item);
            item.download_speed_bps = estimate.speed_bps;
            item.eta_seconds = estimate.eta_seconds;
        }

        self.histories.retain(|gid, history| {
            live_gids.contains(gid)
                || history
                    .last_seen
                    .is_some_and(|seen| now.saturating_duration_since(seen) <= HISTORY_RETENTION)
        });
    }
}

#[derive(Debug, Clone, Copy)]
struct TransferEstimate {
    speed_bps: u64,
    eta_seconds: Option<u64>,
}

impl TransferHistory {
    fn mark_inactive(&mut self) {
        self.active = false;
        self.samples.clear();
        self.last_refresh = None;
        self.last_progress = None;
        self.displayed_eta = None;
        self.shock_count = 0;
        self.shock_direction = 0;
    }

    fn update(
        &mut self,
        now: Instant,
        completed_bytes: u64,
        realtime_speed_bps: u64,
        item: &DownloadItem,
    ) -> TransferEstimate {
        let resumed = !self.active;
        self.active = true;
        if resumed {
            // Never average a scheduler pause into the transfer rate. Keep the
            // learned long-term rate only as a weak prior after resuming.
            self.samples.clear();
            self.last_refresh = None;
            self.last_progress = Some(now);
            self.displayed_eta = None;
            if realtime_speed_bps > 0 {
                let initial = realtime_speed_bps as f64;
                self.fast_ema = Some(blend_prior(self.fast_ema, initial));
                self.slow_ema = Some(blend_prior(self.slow_ema, initial));
                self.displayed_speed = Some(blend_prior(self.displayed_speed, initial));
            }
        }

        if self
            .samples
            .back()
            .is_some_and(|last| completed_bytes < last.completed_bytes)
        {
            // A retry can reuse a GID while resetting its byte counter.
            self.samples.clear();
            self.recent_rates.clear();
            self.fast_ema = None;
            self.slow_ema = None;
            self.displayed_speed = None;
            self.displayed_eta = None;
        }

        let previous = self.samples.back().copied();
        if previous.is_none_or(|sample| sample.at != now) {
            self.samples.push_back(TransferSample {
                at: now,
                completed_bytes,
            });
        }
        self.prune_samples(now);

        let elapsed = self
            .last_refresh
            .map(|last| now.saturating_duration_since(last).as_secs_f64())
            .unwrap_or(0.0);
        self.last_refresh = Some(now);

        if let Some(previous) = previous {
            let sample_elapsed = now.saturating_duration_since(previous.at).as_secs_f64();
            if completed_bytes > previous.completed_bytes {
                self.last_progress = Some(now);
            }
            if sample_elapsed > 0.0 {
                let byte_rate = completed_bytes.saturating_sub(previous.completed_bytes) as f64
                    / sample_elapsed;
                // Byte deltas are authoritative. aria2's instantaneous value is
                // only used to bridge coarse counters and receives a small vote.
                let observed = if byte_rate == 0.0 && realtime_speed_bps > 0 {
                    realtime_speed_bps as f64
                } else if realtime_speed_bps > 0 {
                    byte_rate * 0.9 + realtime_speed_bps as f64 * 0.1
                } else {
                    byte_rate
                };
                let robust = self.robust_rate(observed);
                self.update_emas(robust, sample_elapsed);
            }
        } else if realtime_speed_bps > 0 {
            let initial = realtime_speed_bps as f64;
            self.fast_ema.get_or_insert(initial);
            self.slow_ema.get_or_insert(initial);
            self.displayed_speed.get_or_insert(initial);
        }

        let stalled_for = self
            .last_progress
            .map(|progress| now.saturating_duration_since(progress))
            .unwrap_or_default();
        let hard_stalled = realtime_speed_bps == 0 && stalled_for >= STALL_ZERO_AFTER;
        let target = if hard_stalled {
            0.0
        } else {
            self.combined_rate().unwrap_or(realtime_speed_bps as f64)
        };
        let displayed = self.slew_limited_speed(target, elapsed, item);
        let speed_bps = if hard_stalled {
            self.displayed_speed = Some(0.0);
            0
        } else {
            displayed.round().max(0.0) as u64
        };
        let eta_seconds = self.smooth_eta(elapsed, item, speed_bps, hard_stalled);
        TransferEstimate {
            speed_bps,
            eta_seconds,
        }
    }

    fn robust_rate(&mut self, observed: f64) -> f64 {
        if !observed.is_finite() || observed < 0.0 {
            return 0.0;
        }
        if observed == 0.0 {
            self.shock_count = 0;
            self.shock_direction = 0;
            self.push_rate(observed);
            return observed;
        }

        let positive = self
            .recent_rates
            .iter()
            .copied()
            .filter(|rate| *rate > 0.0)
            .collect::<Vec<_>>();
        if positive.len() < 5 {
            self.push_rate(observed);
            return observed;
        }

        let center = median(&positive);
        let deviations = positive
            .iter()
            .map(|rate| (rate - center).abs())
            .collect::<Vec<_>>();
        let mad = median(&deviations);
        let mut sorted = positive;
        sorted.sort_by(f64::total_cmp);
        let first_quartile = sorted[sorted.len() / 4];
        let third_quartile = sorted[sorted.len() * 3 / 4];
        let interquartile_range = third_quartile - first_quartile;
        let (low, high) = if interquartile_range > center * 0.05 {
            // A wide or bimodal connection is genuinely bursty. Tukey fences
            // retain both modes instead of mistaking every high burst for an
            // outlier when the rolling sample count happens to be odd.
            (
                (first_quartile - interquartile_range * 1.5).max(0.0),
                third_quartile + interquartile_range * 1.5,
            )
        } else {
            let robust_sigma = (mad * 1.4826).max(center * 0.08);
            (
                (center - robust_sigma * 4.0).max(center * 0.15),
                (center + robust_sigma * 4.0).max(center * 1.75),
            )
        };
        let direction = if observed < low {
            -1
        } else if observed > high {
            1
        } else {
            0
        };

        let accepted = if direction == 0 {
            self.shock_count = 0;
            self.shock_direction = 0;
            observed
        } else {
            if direction == self.shock_direction {
                self.shock_count = self.shock_count.saturating_add(1);
            } else {
                self.shock_direction = direction;
                self.shock_count = 1;
            }
            if self.shock_count >= CHANGE_CONFIRMATION_SAMPLES {
                // Repeated outliers are a change point, not noise.
                self.recent_rates.clear();
                self.shock_count = 0;
                self.shock_direction = 0;
                observed
            } else {
                observed.clamp(low, high)
            }
        };
        self.push_rate(observed);
        accepted
    }

    fn push_rate(&mut self, rate: f64) {
        self.recent_rates.push_back(rate);
        while self.recent_rates.len() > MAX_RATE_SAMPLES {
            self.recent_rates.pop_front();
        }
    }

    fn update_emas(&mut self, sample: f64, elapsed: f64) {
        self.fast_ema = Some(ema(
            self.fast_ema,
            sample,
            exp_alpha(elapsed, FAST_TAU_SECONDS),
        ));
        self.slow_ema = Some(ema(
            self.slow_ema,
            sample,
            exp_alpha(elapsed, SLOW_TAU_SECONDS),
        ));
    }

    fn combined_rate(&self) -> Option<f64> {
        match (self.fast_ema, self.slow_ema) {
            (Some(fast), Some(slow)) => {
                let ratio = fast / slow.max(1.0);
                let fast_weight = if !(0.72..=1.38).contains(&ratio) {
                    0.62
                } else {
                    0.28
                };
                Some(fast * fast_weight + slow * (1.0 - fast_weight))
            }
            (Some(rate), None) | (None, Some(rate)) => Some(rate),
            (None, None) => None,
        }
    }

    fn slew_limited_speed(&mut self, target: f64, elapsed: f64, item: &DownloadItem) -> f64 {
        let Some(previous) = self.displayed_speed else {
            self.displayed_speed = Some(target.max(0.0));
            return target.max(0.0);
        };
        if previous <= 0.0 || target <= 0.0 || elapsed <= 0.0 {
            self.displayed_speed = Some(target.max(0.0));
            return target.max(0.0);
        }

        let raw_eta =
            item.total_bytes.saturating_sub(item.completed_bytes) as f64 / target.max(1.0);
        let urgency = if raw_eta < 20.0 { 2.5 } else { 1.0 };
        // Symmetric log-space limits avoid biasing an alternating connection
        // toward its troughs.
        let max_up = (0.15 * urgency * elapsed.min(5.0)).exp();
        let max_down = (-0.15 * urgency * elapsed.min(5.0)).exp();
        let limited = target.clamp(previous * max_down, previous * max_up);
        self.displayed_speed = Some(limited);
        limited
    }

    fn smooth_eta(
        &mut self,
        elapsed: f64,
        item: &DownloadItem,
        speed_bps: u64,
        hard_stalled: bool,
    ) -> Option<u64> {
        let remaining = item.total_bytes.checked_sub(item.completed_bytes)?;
        if remaining == 0 {
            self.displayed_eta = Some(0.0);
            return Some(0);
        }
        if hard_stalled || speed_bps == 0 {
            self.displayed_eta = None;
            return None;
        }

        let raw = remaining as f64 / speed_bps as f64;
        let predicted = self
            .displayed_eta
            .map(|eta| (eta - elapsed).max(0.0))
            .unwrap_or(raw);
        let tau = if raw > predicted {
            ETA_WORSENING_TAU_SECONDS
        } else {
            ETA_IMPROVEMENT_TAU_SECONDS
        };
        let alpha = if self.displayed_eta.is_none() {
            1.0
        } else {
            exp_alpha(elapsed, tau)
        };
        let smoothed = predicted + (raw - predicted) * alpha;
        self.displayed_eta = Some(smoothed);
        Some(smoothed.ceil().max(1.0) as u64)
    }

    fn prune_samples(&mut self, now: Instant) {
        while self.samples.len() > 1
            && self
                .samples
                .front()
                .is_some_and(|sample| now.saturating_duration_since(sample.at) > SAMPLE_RETENTION)
        {
            self.samples.pop_front();
        }
    }
}

fn blend_prior(prior: Option<f64>, current: f64) -> f64 {
    prior.map_or(current, |prior| prior * 0.35 + current * 0.65)
}

fn exp_alpha(elapsed: f64, tau: f64) -> f64 {
    if elapsed <= 0.0 {
        0.0
    } else {
        1.0 - (-elapsed / tau).exp()
    }
}

fn ema(previous: Option<f64>, sample: f64, alpha: f64) -> f64 {
    previous.map_or(sample, |previous| previous + alpha * (sample - previous))
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_item(total: u64, completed: u64, speed: u64) -> DownloadItem {
        DownloadItem {
            gid: "gid-alpha".into(),
            status: DownloadStatus::Active,
            name: "alpha".into(),
            primary_path: None,
            source_uri: None,
            info_hash: None,
            num_seeders: None,
            followed_by: Vec::new(),
            belongs_to: None,
            is_metadata_only: false,
            total_bytes: total,
            completed_bytes: completed,
            download_speed_bps: speed,
            realtime_download_speed_bps: speed,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: None,
            error_code: None,
            error_message: None,
            batch: None,
            queue_held: false,
        }
    }

    #[test]
    fn one_second_spike_is_heavily_smoothed() {
        let base = Instant::now();
        let mut tracker = RollingSpeedTracker::default();
        let mut items = vec![active_item(100_000, 0, 1_000)];
        tracker.refresh(base, &mut items);
        for second in 1..=6 {
            items[0].completed_bytes = second * 1_000;
            items[0].download_speed_bps = 1_000;
            tracker.refresh(base + Duration::from_secs(second), &mut items);
        }
        items[0].completed_bytes += 10_000;
        items[0].download_speed_bps = 10_000;
        tracker.refresh(base + Duration::from_secs(7), &mut items);
        assert!(items[0].download_speed_bps < 1_500);
        assert_eq!(items[0].realtime_download_speed_bps, 10_000);
    }

    #[test]
    fn sustained_change_is_accepted_and_converges() {
        let base = Instant::now();
        let mut tracker = RollingSpeedTracker::default();
        let mut items = vec![active_item(1_000_000, 0, 1_000)];
        tracker.refresh(base, &mut items);
        for second in 1..=10 {
            items[0].completed_bytes += 1_000;
            items[0].download_speed_bps = 1_000;
            tracker.refresh(base + Duration::from_secs(second), &mut items);
        }
        for second in 11..=30 {
            items[0].completed_bytes += 4_000;
            items[0].download_speed_bps = 4_000;
            tracker.refresh(base + Duration::from_secs(second), &mut items);
        }
        assert!(items[0].download_speed_bps > 2_500);
        assert!(items[0].download_speed_bps < 4_100);
    }

    #[test]
    fn brief_stall_preserves_a_useful_eta_but_sustained_stall_clears_it() {
        let base = Instant::now();
        let mut tracker = RollingSpeedTracker::default();
        let mut items = vec![active_item(100_000, 0, 1_000)];
        tracker.refresh(base, &mut items);
        items[0].completed_bytes = 5_000;
        items[0].download_speed_bps = 1_000;
        tracker.refresh(base + Duration::from_secs(5), &mut items);
        items[0].download_speed_bps = 0;
        tracker.refresh(base + Duration::from_secs(10), &mut items);
        assert!(items[0].download_speed_bps > 0);
        assert!(items[0].eta_seconds.is_some());
        items[0].download_speed_bps = 0;
        tracker.refresh(base + Duration::from_secs(26), &mut items);
        assert_eq!(items[0].download_speed_bps, 0);
        assert_eq!(items[0].eta_seconds, None);
    }

    #[test]
    fn pause_time_is_not_averaged_into_resumed_speed() {
        let base = Instant::now();
        let mut tracker = RollingSpeedTracker::default();
        let mut items = vec![active_item(100_000, 0, 1_000)];
        tracker.refresh(base, &mut items);
        items[0].completed_bytes = 2_000;
        tracker.refresh(base + Duration::from_secs(2), &mut items);
        items[0].status = DownloadStatus::Paused;
        items[0].download_speed_bps = 0;
        tracker.refresh(base + Duration::from_secs(60), &mut items);
        items[0].status = DownloadStatus::Active;
        items[0].download_speed_bps = 1_000;
        tracker.refresh(base + Duration::from_secs(61), &mut items);
        assert!(items[0].download_speed_bps >= 900);
    }

    #[test]
    fn eta_deadline_moves_less_than_the_raw_speed_ratio() {
        let base = Instant::now();
        let mut tracker = RollingSpeedTracker::default();
        let mut items = vec![active_item(100_000, 0, 1_000)];
        tracker.refresh(base, &mut items);
        let initial_eta = items[0].eta_seconds.unwrap();
        items[0].completed_bytes = 500;
        items[0].download_speed_bps = 500;
        tracker.refresh(base + Duration::from_secs(1), &mut items);
        let eta = items[0].eta_seconds.unwrap();
        assert!(eta < initial_eta * 2);
        assert!(eta >= initial_eta.saturating_sub(1));
    }

    #[test]
    fn alternating_network_bursts_converge_near_the_mean() {
        let base = Instant::now();
        let mut tracker = RollingSpeedTracker::default();
        let mut items = vec![active_item(1_000_000, 0, 1_000)];
        tracker.refresh(base, &mut items);
        for second in 1..=40 {
            let rate = if second % 2 == 0 { 1_600 } else { 400 };
            items[0].completed_bytes += rate;
            items[0].download_speed_bps = rate;
            tracker.refresh(base + Duration::from_secs(second), &mut items);
        }
        assert!(
            (850..=1_150).contains(&items[0].download_speed_bps),
            "smoothed speed was {}",
            items[0].download_speed_bps
        );
    }

    #[test]
    fn byte_counter_reset_starts_a_fresh_estimate() {
        let base = Instant::now();
        let mut tracker = RollingSpeedTracker::default();
        let mut items = vec![active_item(100_000, 50_000, 1_000)];
        tracker.refresh(base, &mut items);
        items[0].completed_bytes = 52_000;
        tracker.refresh(base + Duration::from_secs(2), &mut items);
        items[0].completed_bytes = 0;
        items[0].download_speed_bps = 2_000;
        tracker.refresh(base + Duration::from_secs(3), &mut items);
        assert_eq!(items[0].realtime_download_speed_bps, 2_000);
        assert_eq!(items[0].download_speed_bps, 2_000);
        assert!(items[0].eta_seconds.is_some());
    }
}
