use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Local, Timelike};

use crate::daemon::{DownloadItem, DownloadStatus, QueueBatchTarget, Snapshot};
use crate::state::ManualOrScheduled;

const HORIZON_SECONDS: u64 = 24 * 365 * 3600;
const PEER_NAME_LIMIT: usize = 3;
const MAX_RECORDED_PHASES: usize = 96;
const COLD_START_UTILIZATION: f64 = 0.80;
const EPSILON: f64 = 1e-9;

#[derive(Debug, Clone)]
pub(crate) struct ScheduledEtaProjection {
    pub eta_seconds: u64,
    pub projected_now_speed_bps: u64,
    pub phase_count: usize,
    pub phases: Vec<ScheduledEtaPhase>,
}

#[derive(Debug, Clone)]
pub(crate) struct ScheduledEtaPhase {
    pub start_offset_seconds: u64,
    pub duration_seconds: u64,
    pub projected_item_speed_bps: u64,
    pub projected_aggregate_speed_bps: u64,
    pub peer_count: usize,
    pub peer_names: Vec<String>,
    pub end: ProjectionPhaseEnd,
}

#[derive(Debug, Clone)]
pub(crate) enum ProjectionPhaseEnd {
    HourBoundary,
    PeerCompleted { name: String },
    SelectedCompleted,
}

#[derive(Debug, Clone)]
struct SimDownload {
    gid: String,
    name: String,
    batch: QueueBatchTarget,
    remaining_bytes: f64,
    weight: f64,
    was_active: bool,
    order: usize,
}

/// A calibrated model of usable aggregate bandwidth. `ceiling_bps` is either
/// the configured usual Internet speed, an observed uncapped speed, or the
/// current cap when no observations exist. `utilization` preserves protocol,
/// server, and connection overhead when projecting into another schedule slot.
#[derive(Debug, Clone, Copy)]
struct CapacityModel {
    ceiling_bps: f64,
    utilization: f64,
}

impl CapacityModel {
    fn from_snapshot(snapshot: &Snapshot) -> Option<Self> {
        let observed = snapshot
            .current_downloads
            .iter()
            .filter(|item| item.status == DownloadStatus::Active)
            .map(|item| item.download_speed_bps as f64)
            .sum::<f64>();
        let usual = snapshot
            .scheduler
            .usual_internet_speed_bps
            .map(|v| v as f64);
        let current_limit = snapshot.scheduler.effective_limit_bps.map(|v| v as f64);

        let inferred_ceiling = if current_limit.is_some() {
            // When the current hour is capped, the observation cannot tell us
            // the uncapped line rate. A larger configured future cap is still
            // useful evidence; utilization below preserves measured overhead.
            snapshot
                .scheduler
                .schedule_limits_bps
                .iter()
                .flatten()
                .copied()
                .max()
                .map(|limit| limit as f64)
                .unwrap_or(observed)
        } else {
            observed
        };
        let ceiling = usual.unwrap_or(inferred_ceiling).max(observed);
        if ceiling <= 0.0 {
            return None;
        }
        let currently_available = current_limit.map_or(ceiling, |limit| limit.min(ceiling));
        let utilization = if observed > 0.0 && currently_available > 0.0 {
            (observed / currently_available).clamp(0.05, 1.0)
        } else {
            COLD_START_UTILIZATION
        };
        Some(Self {
            ceiling_bps: ceiling,
            utilization,
        })
    }

    fn speed_for_limit(self, limit: Option<u64>) -> f64 {
        let available = limit.map_or(self.ceiling_bps, |limit| self.ceiling_bps.min(limit as f64));
        available * self.utilization
    }
}

/// Simulate the queue as a small discrete-event system. Unlike the former
/// active-only projection, this includes slot backfilling, scheduler-held
/// batches, waiting downloads, fair redistribution after peers finish, hourly
/// caps, and cold-start estimates when a just-started item has no rate yet.
pub(crate) fn project_scheduled_eta(
    now: DateTime<Local>,
    snapshot: &Snapshot,
    item: &DownloadItem,
) -> Option<ScheduledEtaProjection> {
    if snapshot.scheduler.mode != ManualOrScheduled::Scheduled {
        return None;
    }
    let selected_remaining = remaining_bytes(item)?;
    if selected_remaining == 0 {
        return Some(ScheduledEtaProjection {
            eta_seconds: 0,
            projected_now_speed_bps: 0,
            phase_count: 0,
            phases: Vec::new(),
        });
    }
    if !is_projectable(item) {
        return None;
    }

    let capacity = CapacityModel::from_snapshot(snapshot)?;
    let active_rates = snapshot
        .current_downloads
        .iter()
        .filter(|item| item.status == DownloadStatus::Active && item.download_speed_bps > 0)
        .map(|item| item.download_speed_bps as f64)
        .collect::<Vec<_>>();
    let typical_weight = if active_rates.is_empty() {
        capacity.ceiling_bps / f64::from(snapshot.queue.slots.max(1))
    } else {
        median(&active_rates)
    }
    .max(1.0);

    let mut downloads = snapshot
        .current_downloads
        .iter()
        .enumerate()
        .filter_map(|(order, candidate)| {
            if !is_projectable(candidate) {
                return None;
            }
            Some(SimDownload {
                gid: candidate.gid.clone(),
                name: candidate.name.clone(),
                batch: QueueBatchTarget::of(candidate.batch),
                remaining_bytes: remaining_bytes(candidate)? as f64,
                weight: if candidate.status == DownloadStatus::Active
                    && candidate.download_speed_bps > 0
                {
                    candidate.download_speed_bps as f64
                } else {
                    typical_weight
                },
                was_active: candidate.status == DownloadStatus::Active,
                order,
            })
        })
        .collect::<Vec<_>>();
    if !downloads.iter().any(|download| download.gid == item.gid) {
        return None;
    }

    let batch_order = projected_batch_order(snapshot, &downloads);
    let slots = usize::from(snapshot.queue.slots.max(1));
    let mut elapsed_seconds = 0.0f64;
    let mut hour = now.hour() as usize;
    let seconds_past_hour = now.minute() as f64 * 60.0
        + now.second() as f64
        + now.nanosecond() as f64 / 1_000_000_000.0;
    let mut seconds_until_boundary = (3600.0 - seconds_past_hour).max(0.001);
    let mut projected_now_speed_bps = None;
    let mut phase_count = 0usize;
    let mut phases = Vec::new();

    while elapsed_seconds <= HORIZON_SECONDS as f64 + EPSILON {
        let batch = batch_order.iter().copied().find(|batch| {
            downloads
                .iter()
                .any(|download| download.batch == *batch && download.remaining_bytes > EPSILON)
        })?;
        let running = running_indexes(&downloads, batch, slots);
        if running.is_empty() {
            return None;
        }

        let aggregate_speed =
            capacity.speed_for_limit(snapshot.scheduler.schedule_limits_bps[hour]);
        if aggregate_speed <= EPSILON {
            elapsed_seconds += seconds_until_boundary;
            hour = projected_hour(now, elapsed_seconds);
            seconds_until_boundary = 3600.0;
            continue;
        }
        let total_weight = running
            .iter()
            .map(|index| downloads[*index].weight)
            .sum::<f64>();
        if total_weight <= EPSILON {
            return None;
        }
        let speeds = running
            .iter()
            .map(|index| {
                (
                    *index,
                    aggregate_speed * downloads[*index].weight / total_weight,
                )
            })
            .collect::<Vec<_>>();
        let selected_speed = speeds
            .iter()
            .find(|(index, _)| downloads[*index].gid == item.gid)
            .map(|(_, speed)| *speed)
            .unwrap_or(0.0);
        projected_now_speed_bps.get_or_insert_with(|| round_speed(selected_speed));

        let earliest_completion = speeds
            .iter()
            .map(|(index, speed)| downloads[*index].remaining_bytes / speed.max(EPSILON))
            .fold(f64::INFINITY, f64::min);
        let phase_duration = seconds_until_boundary.min(earliest_completion);
        if !phase_duration.is_finite() || phase_duration <= EPSILON {
            return None;
        }

        let peer_names = running
            .iter()
            .filter(|index| downloads[**index].gid != item.gid)
            .map(|index| downloads[*index].name.clone())
            .take(PEER_NAME_LIMIT)
            .collect::<Vec<_>>();
        let selected_is_running = selected_speed > 0.0;
        let peer_count = running
            .len()
            .saturating_sub(usize::from(selected_is_running));

        for (index, speed) in &speeds {
            downloads[*index].remaining_bytes =
                (downloads[*index].remaining_bytes - speed * phase_duration).max(0.0);
        }
        let completed = speeds
            .iter()
            .filter_map(|(index, _)| {
                (downloads[*index].remaining_bytes <= EPSILON).then_some(*index)
            })
            .collect::<Vec<_>>();
        let selected_completed = completed
            .iter()
            .any(|index| downloads[*index].gid == item.gid);
        let peer_completion_name = completed
            .iter()
            .find(|index| downloads[**index].gid != item.gid)
            .map(|index| downloads[*index].name.clone());
        let hit_boundary = seconds_until_boundary <= phase_duration + EPSILON;
        let end = if selected_completed {
            ProjectionPhaseEnd::SelectedCompleted
        } else if let Some(name) = peer_completion_name {
            ProjectionPhaseEnd::PeerCompleted { name }
        } else {
            ProjectionPhaseEnd::HourBoundary
        };

        phase_count += 1;
        if phases.len() < MAX_RECORDED_PHASES {
            phases.push(ScheduledEtaPhase {
                start_offset_seconds: elapsed_seconds.ceil() as u64,
                duration_seconds: phase_duration.ceil().max(1.0) as u64,
                projected_item_speed_bps: round_speed(selected_speed),
                projected_aggregate_speed_bps: round_speed(aggregate_speed),
                peer_count,
                peer_names,
                end,
            });
        }
        elapsed_seconds += phase_duration;
        if selected_completed {
            return Some(ScheduledEtaProjection {
                eta_seconds: elapsed_seconds.ceil() as u64,
                projected_now_speed_bps: projected_now_speed_bps.unwrap_or(0),
                phase_count,
                phases,
            });
        }
        if hit_boundary {
            // Derive the next local hour from an absolute instant. This handles
            // daylight-saving skipped and repeated hours correctly.
            hour = projected_hour(now, elapsed_seconds);
            seconds_until_boundary = 3600.0;
        } else {
            seconds_until_boundary -= phase_duration;
        }
    }
    None
}

fn is_projectable(item: &DownloadItem) -> bool {
    matches!(
        item.status,
        DownloadStatus::Active | DownloadStatus::Waiting
    ) || (item.status == DownloadStatus::Paused && item.queue_held)
}

fn remaining_bytes(item: &DownloadItem) -> Option<u64> {
    item.total_bytes.checked_sub(item.completed_bytes)
}

fn projected_batch_order(snapshot: &Snapshot, downloads: &[SimDownload]) -> Vec<QueueBatchTarget> {
    let mut batches = downloads
        .iter()
        .map(|download| download.batch)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(active) = snapshot.queue.active_batch
        && let Some(position) = batches.iter().position(|batch| *batch == active)
    {
        batches.remove(position);
        batches.insert(0, active);
    }
    batches
}

fn running_indexes(downloads: &[SimDownload], batch: QueueBatchTarget, slots: usize) -> Vec<usize> {
    let mut candidates = downloads
        .iter()
        .enumerate()
        .filter(|(_, download)| download.batch == batch && download.remaining_bytes > EPSILON)
        .map(|(index, download)| (index, !download.was_active, download.order))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(_, was_not_active, order)| (*was_not_active, *order));
    let observed_active = candidates
        .iter()
        .filter(|(index, _, _)| downloads[*index].was_active)
        .count();
    candidates
        .into_iter()
        .take(slots.max(observed_active))
        .map(|(index, _, _)| index)
        .collect()
}

fn round_speed(speed_bps: f64) -> u64 {
    speed_bps.round().max(0.0) as u64
}

fn projected_hour(now: DateTime<Local>, elapsed_seconds: f64) -> usize {
    let millis = (elapsed_seconds * 1_000.0).round().min(i64::MAX as f64) as i64;
    (now + Duration::milliseconds(millis)).hour() as usize
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
    use chrono::{Local, TimeZone};

    use super::*;
    use crate::daemon::snapshot::{QueueSnapshot, SchedulerSnapshot, Snapshot};
    use crate::state::CancelBehaviorPreference;

    fn item(
        name: &str,
        remaining: u64,
        speed: u64,
        status: DownloadStatus,
        batch: Option<u32>,
        held: bool,
    ) -> DownloadItem {
        DownloadItem {
            gid: format!("gid-{name}"),
            status,
            name: name.into(),
            primary_path: None,
            source_uri: None,
            info_hash: None,
            num_seeders: None,
            followed_by: Vec::new(),
            belongs_to: None,
            is_metadata_only: false,
            total_bytes: remaining,
            completed_bytes: 0,
            download_speed_bps: speed,
            realtime_download_speed_bps: speed,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: None,
            error_code: None,
            error_message: None,
            batch,
            queue_held: held,
        }
    }

    fn snapshot_with(
        effective_limit: Option<u64>,
        usual: Option<u64>,
        schedule: [Option<u64>; 24],
        slots: u8,
        active_batch: Option<QueueBatchTarget>,
        downloads: Vec<DownloadItem>,
    ) -> Snapshot {
        let mut snapshot = Snapshot::empty(
            "socket".into(),
            "state".into(),
            "config".into(),
            "exe".into(),
            "build".into(),
        );
        snapshot.scheduler = SchedulerSnapshot {
            mode: ManualOrScheduled::Scheduled,
            manual_limit_bps: None,
            usual_internet_speed_bps: usual,
            schedule_limits_bps: schedule,
            effective_limit_bps: effective_limit,
            current_hour: 0,
            next_change_at_local: "00:00".into(),
            remembered_cancel_behavior: CancelBehaviorPreference::Ask,
        };
        snapshot.queue = QueueSnapshot {
            slots,
            active_batch,
            ..QueueSnapshot::default()
        };
        snapshot.current_downloads = downloads;
        snapshot
    }

    #[test]
    fn schedule_boundary_changes_completion_rate() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 59, 50).unwrap();
        let mut schedule = [Some(100); 24];
        schedule[11] = Some(200);
        let selected = item("alpha", 2_400, 100, DownloadStatus::Active, Some(0), false);
        let snapshot = snapshot_with(
            Some(100),
            None,
            schedule,
            1,
            Some(QueueBatchTarget::Number(0)),
            vec![selected.clone()],
        );
        let projection = project_scheduled_eta(now, &snapshot, &selected).unwrap();
        assert_eq!(projection.eta_seconds, 17);
        assert_eq!(projection.phases.len(), 2);
        assert_eq!(projection.phases[1].projected_item_speed_bps, 200);
    }

    #[test]
    fn later_batch_eta_includes_earlier_batch_and_slot_backfill() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let first = item("first", 100, 100, DownloadStatus::Active, Some(0), false);
        let second = item("second", 100, 0, DownloadStatus::Waiting, Some(0), false);
        let selected = item("later", 100, 0, DownloadStatus::Paused, Some(1), true);
        let snapshot = snapshot_with(
            Some(100),
            Some(100),
            [Some(100); 24],
            1,
            Some(QueueBatchTarget::Number(0)),
            vec![first, second, selected.clone()],
        );
        let projection = project_scheduled_eta(now, &snapshot, &selected).unwrap();
        assert_eq!(projection.eta_seconds, 3);
        assert_eq!(projection.projected_now_speed_bps, 0);
        assert_eq!(projection.phases.len(), 3);
        assert_eq!(projection.phases[2].projected_item_speed_bps, 100);
    }

    #[test]
    fn slots_share_capacity_and_backfill_in_order() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let short = item("short", 50, 50, DownloadStatus::Active, Some(0), false);
        let long = item("long", 300, 50, DownloadStatus::Active, Some(0), false);
        let selected = item("queued", 100, 0, DownloadStatus::Waiting, Some(0), false);
        let snapshot = snapshot_with(
            Some(100),
            Some(100),
            [Some(100); 24],
            2,
            Some(QueueBatchTarget::Number(0)),
            vec![short, long, selected.clone()],
        );
        let projection = project_scheduled_eta(now, &snapshot, &selected).unwrap();
        assert_eq!(projection.projected_now_speed_bps, 0);
        assert_eq!(projection.eta_seconds, 3);
        assert!(matches!(
            projection.phases[0].end,
            ProjectionPhaseEnd::PeerCompleted { .. }
        ));
    }

    #[test]
    fn manual_pause_is_not_assumed_to_resume() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let selected = item("paused", 100, 0, DownloadStatus::Paused, Some(0), false);
        let snapshot = snapshot_with(
            Some(100),
            Some(100),
            [Some(100); 24],
            1,
            None,
            vec![selected.clone()],
        );
        assert!(project_scheduled_eta(now, &snapshot, &selected).is_none());
    }

    #[test]
    fn cold_start_uses_conservative_fraction_of_known_capacity() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let selected = item("starting", 800, 0, DownloadStatus::Active, Some(0), false);
        let snapshot = snapshot_with(
            Some(1_000),
            Some(1_000),
            [Some(1_000); 24],
            1,
            Some(QueueBatchTarget::Number(0)),
            vec![selected.clone()],
        );
        let projection = project_scheduled_eta(now, &snapshot, &selected).unwrap();
        assert_eq!(projection.projected_now_speed_bps, 800);
        assert_eq!(projection.eta_seconds, 1);
    }

    #[test]
    fn freed_bandwidth_is_redistributed_without_selected_row_bias() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let selected = item("alpha", 1_000, 100, DownloadStatus::Active, Some(0), false);
        let short = item("short", 100, 100, DownloadStatus::Active, Some(0), false);
        let long = item("long", 10_000, 300, DownloadStatus::Active, Some(0), false);
        let snapshot = snapshot_with(
            Some(500),
            Some(500),
            [Some(500); 24],
            3,
            Some(QueueBatchTarget::Number(0)),
            vec![selected.clone(), short, long.clone()],
        );
        let selected_projection = project_scheduled_eta(now, &snapshot, &selected).unwrap();
        let long_projection = project_scheduled_eta(now, &snapshot, &long).unwrap();
        assert_eq!(selected_projection.phases[1].projected_item_speed_bps, 125);
        assert_eq!(long_projection.phases[1].projected_item_speed_bps, 375);
    }

    #[test]
    fn explicitly_started_batch_runs_before_lower_held_batch() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let selected = item("lower", 100, 0, DownloadStatus::Paused, Some(0), true);
        let current = item("started", 100, 100, DownloadStatus::Active, Some(2), false);
        let snapshot = snapshot_with(
            Some(100),
            Some(100),
            [Some(100); 24],
            1,
            Some(QueueBatchTarget::Number(2)),
            vec![selected.clone(), current],
        );
        let projection = project_scheduled_eta(now, &snapshot, &selected).unwrap();
        assert_eq!(projection.eta_seconds, 2);
        assert_eq!(projection.projected_now_speed_bps, 0);
    }

    #[test]
    fn unassigned_batch_projects_after_numbered_batches() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let numbered = item("numbered", 100, 100, DownloadStatus::Active, Some(9), false);
        let selected = item("unassigned", 100, 0, DownloadStatus::Paused, None, true);
        let snapshot = snapshot_with(
            Some(100),
            Some(100),
            [Some(100); 24],
            1,
            Some(QueueBatchTarget::Number(9)),
            vec![numbered, selected.clone()],
        );
        assert_eq!(
            project_scheduled_eta(now, &snapshot, &selected)
                .unwrap()
                .eta_seconds,
            2
        );
    }

    #[test]
    fn midnight_wrap_uses_the_next_days_schedule() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 23, 59, 59).unwrap();
        let mut schedule = [Some(100); 24];
        schedule[0] = Some(200);
        let selected = item(
            "overnight",
            300,
            100,
            DownloadStatus::Active,
            Some(0),
            false,
        );
        let snapshot = snapshot_with(
            Some(100),
            Some(200),
            schedule,
            1,
            Some(QueueBatchTarget::Number(0)),
            vec![selected.clone()],
        );
        let projection = project_scheduled_eta(now, &snapshot, &selected).unwrap();
        assert_eq!(projection.eta_seconds, 2);
        assert_eq!(projection.phases[1].projected_item_speed_bps, 200);
    }

    #[test]
    fn manual_mode_and_corrupt_byte_totals_do_not_invent_a_projection() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let mut selected = item("invalid", 100, 100, DownloadStatus::Active, Some(0), false);
        selected.completed_bytes = 101;
        let mut snapshot = snapshot_with(
            Some(100),
            Some(100),
            [Some(100); 24],
            1,
            Some(QueueBatchTarget::Number(0)),
            vec![selected.clone()],
        );
        assert!(project_scheduled_eta(now, &snapshot, &selected).is_none());
        selected.completed_bytes = 0;
        snapshot.current_downloads = vec![selected.clone()];
        snapshot.scheduler.mode = ManualOrScheduled::Manual;
        assert!(project_scheduled_eta(now, &snapshot, &selected).is_none());
    }
}
