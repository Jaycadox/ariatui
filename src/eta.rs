use chrono::{DateTime, Local, Timelike};

use crate::daemon::{DownloadItem, DownloadStatus, Snapshot};
use crate::state::ManualOrScheduled;

const HORIZON_SECONDS: u64 = 24 * 365 * 3600;
const PEER_NAME_LIMIT: usize = 3;
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

#[derive(Debug, Clone, Copy)]
pub(crate) enum AggregateSpeedModel {
    Utilization(f64),
    Observed(u64),
}

#[derive(Debug, Clone)]
struct SimDownload {
    gid: String,
    name: String,
    remaining_bytes: f64,
    weight: f64,
}

pub(crate) fn project_scheduled_eta(
    now: DateTime<Local>,
    snapshot: &Snapshot,
    item: &DownloadItem,
) -> Option<ScheduledEtaProjection> {
    if snapshot.scheduler.mode != ManualOrScheduled::Scheduled {
        return None;
    }
    if item.status != DownloadStatus::Active || item.download_speed_bps == 0 {
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

    let mut downloads = snapshot
        .current_downloads
        .iter()
        .filter_map(sim_download_from_item)
        .collect::<Vec<_>>();
    let mut selected_index = downloads
        .iter()
        .position(|download| download.gid == item.gid)?;

    let observed_total_speed_bps = downloads
        .iter()
        .map(|download| download.weight)
        .sum::<f64>();
    if observed_total_speed_bps <= 0.0 {
        return None;
    }

    let current_cap = min_limit(
        snapshot.scheduler.effective_limit_bps,
        snapshot.scheduler.usual_internet_speed_bps,
    );
    let aggregate_model = match current_cap {
        Some(limit) if limit > 0 => AggregateSpeedModel::Utilization(
            (observed_total_speed_bps / limit as f64).clamp(0.0, 1.0),
        ),
        _ => AggregateSpeedModel::Observed(observed_total_speed_bps.round() as u64),
    };

    let mut projected_now_speed_bps = None;
    let mut phases = Vec::new();
    let mut phase_count = 0usize;
    let mut elapsed_seconds = 0.0f64;
    let mut hour = now.hour() as usize;
    let seconds_past_hour = now.minute() as u64 * 60 + now.second() as u64;
    let mut seconds_until_boundary = (3600 - seconds_past_hour).max(1) as f64;

    while elapsed_seconds <= HORIZON_SECONDS as f64 + EPSILON {
        if downloads.is_empty() {
            return None;
        }
        let slot_cap = min_limit(
            snapshot.scheduler.schedule_limits_bps[hour],
            snapshot.scheduler.usual_internet_speed_bps,
        );
        let aggregate_speed_bps = estimated_aggregate_speed_bps(
            aggregate_model,
            observed_total_speed_bps as u64,
            slot_cap,
        ) as f64;
        if aggregate_speed_bps <= 0.0 {
            elapsed_seconds += seconds_until_boundary;
            hour = (hour + 1) % 24;
            seconds_until_boundary = 3600.0;
            continue;
        }

        let total_weight = downloads
            .iter()
            .map(|download| download.weight)
            .sum::<f64>();
        if total_weight <= 0.0 {
            return None;
        }

        let speeds = downloads
            .iter()
            .map(|download| aggregate_speed_bps * (download.weight / total_weight))
            .collect::<Vec<_>>();
        let selected_speed_bps = speeds[selected_index];
        if projected_now_speed_bps.is_none() {
            projected_now_speed_bps = Some(round_speed(selected_speed_bps));
        }
        if selected_speed_bps <= 0.0 {
            return None;
        }

        let completion_times = downloads
            .iter()
            .zip(speeds.iter())
            .map(|(download, speed)| {
                if *speed <= 0.0 {
                    f64::INFINITY
                } else {
                    download.remaining_bytes / speed
                }
            })
            .collect::<Vec<_>>();
        let earliest_completion_seconds = completion_times
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        if !earliest_completion_seconds.is_finite() {
            elapsed_seconds += seconds_until_boundary;
            hour = (hour + 1) % 24;
            seconds_until_boundary = 3600.0;
            continue;
        }

        let phase_duration_seconds = seconds_until_boundary.min(earliest_completion_seconds);
        if !phase_duration_seconds.is_finite() || phase_duration_seconds <= EPSILON {
            return None;
        }

        let peer_names = downloads
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != selected_index)
            .map(|(_, download)| download.name.clone())
            .take(PEER_NAME_LIMIT)
            .collect::<Vec<_>>();
        let peer_count = downloads.len().saturating_sub(1);

        for (download, speed) in downloads.iter_mut().zip(speeds.iter()) {
            download.remaining_bytes =
                (download.remaining_bytes - (speed * phase_duration_seconds)).max(0.0);
        }

        let completed_indexes = completion_times
            .iter()
            .enumerate()
            .filter_map(|(index, seconds)| {
                if *seconds <= phase_duration_seconds + EPSILON {
                    Some(index)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let selected_completed = completed_indexes.contains(&selected_index);
        let peer_completion_name = completed_indexes
            .iter()
            .copied()
            .filter(|index| *index != selected_index)
            .find_map(|index| downloads.get(index).map(|download| download.name.clone()));
        let end = if selected_completed {
            ProjectionPhaseEnd::SelectedCompleted
        } else if let Some(name) = peer_completion_name {
            ProjectionPhaseEnd::PeerCompleted { name }
        } else {
            ProjectionPhaseEnd::HourBoundary
        };

        phase_count += 1;
        phases.push(ScheduledEtaPhase {
            start_offset_seconds: elapsed_seconds.ceil() as u64,
            duration_seconds: phase_duration_seconds.ceil().max(1.0) as u64,
            projected_item_speed_bps: round_speed(selected_speed_bps),
            projected_aggregate_speed_bps: round_speed(aggregate_speed_bps),
            peer_count,
            peer_names,
            end: end.clone(),
        });

        elapsed_seconds += phase_duration_seconds;
        if selected_completed {
            return Some(ScheduledEtaProjection {
                eta_seconds: elapsed_seconds.ceil() as u64,
                projected_now_speed_bps: projected_now_speed_bps.unwrap_or(0),
                phase_count,
                phases,
            });
        }

        let transferred_weight = completed_indexes
            .iter()
            .copied()
            .filter(|index| *index != selected_index)
            .filter_map(|index| downloads.get(index).map(|download| download.weight))
            .sum::<f64>();
        if transferred_weight > 0.0 {
            downloads[selected_index].weight += transferred_weight;
        }

        for &index in completed_indexes.iter().rev() {
            downloads.remove(index);
        }
        if let Some(new_index) = downloads
            .iter()
            .position(|download| download.gid == item.gid)
        {
            selected_index = new_index;
            if seconds_until_boundary <= phase_duration_seconds + EPSILON {
                hour = (hour + 1) % 24;
                seconds_until_boundary = 3600.0;
            } else {
                seconds_until_boundary -= phase_duration_seconds;
            }
        } else {
            return None;
        }
    }

    None
}

fn sim_download_from_item(item: &DownloadItem) -> Option<SimDownload> {
    if item.status != DownloadStatus::Active || item.download_speed_bps == 0 {
        return None;
    }
    let remaining_bytes = remaining_bytes(item)?;
    if remaining_bytes == 0 {
        return None;
    }
    Some(SimDownload {
        gid: item.gid.clone(),
        name: item.name.clone(),
        remaining_bytes: remaining_bytes as f64,
        weight: item.download_speed_bps as f64,
    })
}

fn remaining_bytes(item: &DownloadItem) -> Option<u64> {
    item.total_bytes.checked_sub(item.completed_bytes)
}

fn estimated_aggregate_speed_bps(
    model: AggregateSpeedModel,
    observed_total_speed_bps: u64,
    scheduled_limit_bps: Option<u64>,
) -> u64 {
    match model {
        AggregateSpeedModel::Utilization(ratio) => {
            if ratio <= 0.0 {
                0
            } else {
                match scheduled_limit_bps {
                    Some(limit) => ((limit as f64 * ratio).round() as u64).max(1),
                    None => observed_total_speed_bps,
                }
            }
        }
        AggregateSpeedModel::Observed(speed) => match scheduled_limit_bps {
            Some(limit) => speed.min(limit),
            None => speed,
        },
    }
}

fn min_limit(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn round_speed(speed_bps: f64) -> u64 {
    speed_bps.round().max(1.0) as u64
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::*;
    use crate::daemon::{
        DownloadItem,
        snapshot::{SchedulerSnapshot, Snapshot},
    };
    use crate::state::{CancelBehaviorPreference, ManualOrScheduled};

    fn active_item(name: &str, remaining_bytes: u64, speed_bps: u64) -> DownloadItem {
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
            total_bytes: remaining_bytes,
            completed_bytes: 0,
            download_speed_bps: speed_bps,
            upload_speed_bps: 0,
            eta_seconds: Some(remaining_bytes / speed_bps.max(1)),
            connections: None,
            error_code: None,
            error_message: None,
        }
    }

    fn with_status(mut item: DownloadItem, status: DownloadStatus, speed_bps: u64) -> DownloadItem {
        item.status = status;
        item.download_speed_bps = speed_bps;
        item
    }

    fn snapshot_with(
        effective_limit_bps: Option<u64>,
        usual_internet_speed_bps: Option<u64>,
        schedule_limits_bps: [Option<u64>; 24],
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
            usual_internet_speed_bps,
            schedule_limits_bps,
            effective_limit_bps,
            current_hour: 0,
            next_change_at_local: "00:00".into(),
            remembered_cancel_behavior: CancelBehaviorPreference::Ask,
        };
        snapshot.current_downloads = downloads;
        snapshot
    }

    #[test]
    fn single_active_download_tracks_schedule_change() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 59, 50).unwrap();
        let mut schedule = [Some(100); 24];
        schedule[11] = Some(200);
        let selected = active_item("alpha.iso", 2_400, 100);
        let snapshot = snapshot_with(Some(100), None, schedule, vec![selected.clone()]);

        let projection = project_scheduled_eta(now, &snapshot, &selected).expect("projection");

        assert_eq!(projection.eta_seconds, 17);
        assert_eq!(projection.projected_now_speed_bps, 100);
        assert_eq!(projection.phase_count, 2);
        assert!(matches!(
            projection.phases[0].end,
            ProjectionPhaseEnd::HourBoundary
        ));
        assert!(matches!(
            projection.phases[1].end,
            ProjectionPhaseEnd::SelectedCompleted
        ));
        assert_eq!(projection.phases[1].projected_item_speed_bps, 200);
    }

    #[test]
    fn smaller_peer_finishing_early_increases_selected_speed() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let schedule = [Some(300); 24];
        let selected = active_item("alpha.iso", 1_000, 100);
        let peer = active_item("beta.iso", 200, 200);
        let snapshot = snapshot_with(Some(300), None, schedule, vec![selected.clone(), peer]);

        let projection = project_scheduled_eta(now, &snapshot, &selected).expect("projection");

        assert_eq!(projection.eta_seconds, 4);
        assert_eq!(projection.phase_count, 2);
        assert_eq!(projection.phases[0].projected_item_speed_bps, 100);
        assert_eq!(projection.phases[1].projected_item_speed_bps, 300);
        assert_eq!(projection.phases[1].peer_count, 0);
        assert!(matches!(
            projection.phases[0].end,
            ProjectionPhaseEnd::PeerCompleted { .. }
        ));
    }

    #[test]
    fn conservative_utilization_preserves_observed_aggregate_after_peer_finishes() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let schedule = [Some(900); 24];
        let selected = active_item("alpha.iso", 1_000, 100);
        let peer = active_item("beta.iso", 200, 200);
        let snapshot = snapshot_with(Some(900), None, schedule, vec![selected.clone(), peer]);

        let projection = project_scheduled_eta(now, &snapshot, &selected).expect("projection");

        assert_eq!(projection.projected_now_speed_bps, 100);
        assert_eq!(projection.phases[1].projected_item_speed_bps, 300);
        assert_ne!(projection.phases[1].projected_item_speed_bps, 900);
    }

    #[test]
    fn scheduler_boundary_before_peer_completion_changes_aggregate_then_peer_finishes() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 59, 58).unwrap();
        let mut schedule = [Some(300); 24];
        schedule[11] = Some(150);
        let selected = active_item("alpha.iso", 3_000, 100);
        let peer = active_item("beta.iso", 1_000, 200);
        let snapshot = snapshot_with(Some(300), None, schedule, vec![selected.clone(), peer]);

        let projection = project_scheduled_eta(now, &snapshot, &selected).expect("projection");

        assert_eq!(projection.phase_count, 3);
        assert!(matches!(
            projection.phases[0].end,
            ProjectionPhaseEnd::HourBoundary
        ));
        assert_eq!(projection.phases[1].projected_aggregate_speed_bps, 150);
        assert!(matches!(
            projection.phases[1].end,
            ProjectionPhaseEnd::PeerCompleted { .. }
        ));
        assert_eq!(projection.phases[2].projected_item_speed_bps, 150);
    }

    #[test]
    fn later_capped_slot_clamps_unlimited_observed_total() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 59, 58).unwrap();
        let mut schedule = [None; 24];
        schedule[11] = Some(150);
        let selected = active_item("alpha.iso", 3_000, 100);
        let peer = active_item("beta.iso", 2_000, 200);
        let snapshot = snapshot_with(None, None, schedule, vec![selected.clone(), peer]);

        let projection = project_scheduled_eta(now, &snapshot, &selected).expect("projection");

        assert_eq!(projection.phases[0].projected_aggregate_speed_bps, 300);
        assert_eq!(projection.phases[1].projected_aggregate_speed_bps, 150);
        assert_eq!(projection.phases[1].projected_item_speed_bps, 50);
    }

    #[test]
    fn finishing_peer_transfers_its_speed_to_selected_download() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let schedule = [Some(500); 24];
        let selected = active_item("alpha.iso", 1_000, 100);
        let fast_peer = active_item("beta.iso", 100, 100);
        let steady_peer = active_item("gamma.iso", 10_000, 300);
        let snapshot = snapshot_with(
            Some(500),
            None,
            schedule,
            vec![selected.clone(), fast_peer, steady_peer],
        );

        let projection = project_scheduled_eta(now, &snapshot, &selected).expect("projection");

        assert!(matches!(
            projection.phases[0].end,
            ProjectionPhaseEnd::PeerCompleted { ref name } if name == "beta.iso"
        ));
        assert_eq!(projection.phases[1].projected_aggregate_speed_bps, 500);
        assert_eq!(projection.phases[1].projected_item_speed_bps, 200);
    }

    #[test]
    fn ignores_waiting_and_paused_downloads() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let schedule = [Some(200); 24];
        let selected = active_item("alpha.iso", 1_000, 100);
        let peer = active_item("beta.iso", 1_000, 100);
        let waiting = with_status(
            active_item("waiting.iso", 500, 500),
            DownloadStatus::Waiting,
            500,
        );
        let paused = with_status(
            active_item("paused.iso", 500, 500),
            DownloadStatus::Paused,
            500,
        );
        let snapshot = snapshot_with(
            Some(200),
            None,
            schedule,
            vec![selected.clone(), peer, waiting, paused],
        );

        let projection = project_scheduled_eta(now, &snapshot, &selected).expect("projection");

        assert_eq!(projection.projected_now_speed_bps, 100);
        assert_eq!(projection.phases[0].peer_count, 1);
        assert_eq!(
            projection.phases[0].peer_names,
            vec!["beta.iso".to_string()]
        );
    }

    #[test]
    fn returns_none_when_selected_item_has_zero_speed() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let schedule = [Some(200); 24];
        let selected = active_item("alpha.iso", 1_000, 0);
        let snapshot = snapshot_with(Some(200), None, schedule, vec![selected.clone()]);

        assert!(project_scheduled_eta(now, &snapshot, &selected).is_none());
    }

    #[test]
    fn final_phase_shows_full_observed_share_after_all_peers_finish() {
        let now = Local.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap();
        let schedule = [Some(200); 24];
        let selected = active_item("alpha.iso", 700, 100);
        let peer_one = active_item("beta.iso", 50, 50);
        let peer_two = active_item("gamma.iso", 50, 50);
        let snapshot = snapshot_with(
            Some(200),
            None,
            schedule,
            vec![selected.clone(), peer_one, peer_two],
        );

        let projection = project_scheduled_eta(now, &snapshot, &selected).expect("projection");
        let final_phase = projection.phases.last().expect("final phase");

        assert_eq!(final_phase.peer_count, 0);
        assert_eq!(final_phase.projected_item_speed_bps, 200);
        assert!(matches!(
            final_phase.end,
            ProjectionPhaseEnd::SelectedCompleted
        ));
    }
}
