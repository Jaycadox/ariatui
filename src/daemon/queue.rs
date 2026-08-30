use std::{collections::HashMap, path::Path};

use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};

use crate::daemon::snapshot::{
    DownloadItem, DownloadStatus, QueueBatchSummary, QueueBatchTarget, QueueSnapshot,
};

/// Per-download queue bookkeeping, persisted so batch assignments survive
/// daemon restarts and aria2 session reloads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueEntry {
    #[serde(default)]
    pub batch: Option<u32>,
    /// True while the batch scheduler (not the user) is responsible for the
    /// pause. Only scheduler-held downloads are auto-resumed when their batch
    /// comes up, so manual pauses are never undone.
    #[serde(default)]
    pub held: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueState {
    entries: HashMap<String, QueueEntry>,
}

impl QueueState {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn batch(&self, gid: &str) -> Option<u32> {
        self.entries.get(gid).and_then(|entry| entry.batch)
    }

    pub fn is_held(&self, gid: &str) -> bool {
        self.entries.get(gid).is_some_and(|entry| entry.held)
    }

    pub fn set_batch(&mut self, gid: &str, batch: Option<u32>) {
        self.entries.entry(gid.to_string()).or_default().batch = batch;
    }

    pub fn set_held(&mut self, gid: &str, held: bool) {
        self.entries.entry(gid.to_string()).or_default().held = held;
    }

    pub fn forget(&mut self, gid: &str) {
        self.entries.remove(gid);
    }

    /// Forget that the scheduler paused anything. Used by pause-all so a later
    /// refresh never resurrects downloads the user stopped on purpose.
    pub fn clear_held(&mut self) -> bool {
        let mut changed = false;
        for entry in self.entries.values_mut() {
            if entry.held {
                entry.held = false;
                changed = true;
            }
        }
        changed
    }

    pub fn retain<F: Fn(&str) -> bool>(&mut self, keep: F) -> bool {
        let before = self.entries.len();
        self.entries.retain(|gid, _| keep(gid));
        self.entries.len() != before
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// A change the batch scheduler wants to make to aria2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueAction {
    /// Pause this download and remember that the scheduler owns the pause, so
    /// it starts again when the batch reaches the front of the queue.
    Hold { gid: String },
    /// Pause this download on the user's behalf. It stays paused until the user
    /// resumes it, even when its batch comes up.
    HoldByUser { gid: String },
    /// The download is already paused by the scheduler. Transfer ownership of
    /// that pause to the user without asking aria2 to pause it a second time.
    KeepPausedByUser { gid: String },
    /// Resume a download the scheduler previously held.
    Release { gid: String },
}

/// Attach persisted queue metadata to aria2's current view. Child downloads
/// inherit their nearest recorded ancestor's batch even when that ancestor has
/// already moved to aria2's stopped list.
pub fn attach_metadata(state: &QueueState, items: &mut [DownloadItem]) {
    let parents = items
        .iter()
        .filter_map(|item| {
            item.belongs_to
                .as_ref()
                .map(|parent| (item.gid.clone(), parent.clone()))
        })
        .collect::<HashMap<_, _>>();

    for item in items {
        item.queue_held = state.is_held(&item.gid);
        item.batch = state.batch(&item.gid);
        if item.batch.is_some() {
            continue;
        }

        let mut ancestor = item.belongs_to.as_deref();
        for _ in 0..=parents.len() {
            let Some(gid) = ancestor else {
                break;
            };
            if let Some(batch) = state.batch(gid) {
                item.batch = Some(batch);
                break;
            }
            ancestor = parents.get(gid).map(String::as_str);
        }
    }
}

/// Batch of a download, following aria2 child downloads (torrent follow-ups)
/// back to the download the user actually queued.
fn effective_batch(by_gid: &HashMap<&str, &DownloadItem>, item: &DownloadItem) -> QueueBatchTarget {
    if item.batch.is_some() {
        return QueueBatchTarget::of(item.batch);
    }
    let mut current = item;
    for _ in 0..4 {
        let Some(parent_gid) = current.belongs_to.as_deref() else {
            break;
        };
        let Some(parent) = by_gid.get(parent_gid) else {
            break;
        };
        current = parent;
    }
    QueueBatchTarget::of(current.batch)
}

fn is_pending(item: &DownloadItem) -> bool {
    matches!(
        item.status,
        DownloadStatus::Active | DownloadStatus::Waiting | DownloadStatus::Paused
    )
}

/// Decide which batch is in play and which downloads should be paused or
/// resumed so batches run one at a time, lowest number first.
///
/// The batch in play is the lowest batch that has an unpaused download. When
/// nothing is unpaused, the scheduler hands the turn to the lowest batch it
/// held earlier. Downloads in later batches wait their turn.
pub fn plan_batch_policy(items: &[DownloadItem]) -> (Option<QueueBatchTarget>, Vec<QueueAction>) {
    let pending = items
        .iter()
        .filter(|item| is_pending(item))
        .collect::<Vec<_>>();
    let by_gid = pending
        .iter()
        .map(|item| (item.gid.as_str(), *item))
        .collect::<HashMap<_, _>>();
    let batch_of = |item: &DownloadItem| effective_batch(&by_gid, item);

    let active_batch = pending
        .iter()
        .filter(|item| !matches!(item.status, DownloadStatus::Paused))
        .map(|item| batch_of(item))
        .min();
    let target = active_batch.or_else(|| {
        pending
            .iter()
            .filter(|item| matches!(item.status, DownloadStatus::Paused) && item.queue_held)
            .map(|item| batch_of(item))
            .min()
    });

    let Some(target) = target else {
        return (None, Vec::new());
    };

    let mut actions = Vec::new();
    for item in &pending {
        let own = batch_of(item);
        match item.status {
            DownloadStatus::Active | DownloadStatus::Waiting if own > target => {
                actions.push(QueueAction::Hold {
                    gid: item.gid.clone(),
                });
            }
            DownloadStatus::Paused if item.queue_held && own == target => {
                actions.push(QueueAction::Release {
                    gid: item.gid.clone(),
                });
            }
            _ => {}
        }
    }
    (Some(target), actions)
}

/// Batch labels with counts, lowest first, for the queue overview surfaces.
pub fn summarize_queue(
    slots: u8,
    active_batch: Option<QueueBatchTarget>,
    items: &[DownloadItem],
) -> QueueSnapshot {
    let by_gid = items
        .iter()
        .map(|item| (item.gid.as_str(), item))
        .collect::<HashMap<_, _>>();
    let mut batches: Vec<QueueBatchSummary> = Vec::new();
    let mut held_count = 0usize;
    let mut pending_count = 0usize;

    for item in items.iter().filter(|item| is_pending(item)) {
        if item.belongs_to.as_deref().is_some_and(|parent_gid| {
            by_gid
                .get(parent_gid)
                .is_some_and(|parent| is_pending(parent))
        }) {
            continue;
        }
        let target = effective_batch(&by_gid, item);
        if !batches.iter().any(|batch| batch.target == target) {
            batches.push(QueueBatchSummary {
                target,
                running: 0,
                waiting: 0,
                paused: 0,
                held: 0,
            });
        }
        let summary = batches
            .iter_mut()
            .find(|batch| batch.target == target)
            .expect("batch summary was just ensured");
        match item.status {
            DownloadStatus::Active => summary.running += 1,
            DownloadStatus::Waiting => summary.waiting += 1,
            DownloadStatus::Paused if item.queue_held => {
                summary.held += 1;
                held_count += 1;
            }
            DownloadStatus::Paused => summary.paused += 1,
            _ => {}
        }
        pending_count += 1;
    }

    batches.sort_by_key(|batch| batch.target);
    QueueSnapshot {
        slots,
        active_batch,
        batches,
        held_count,
        pending_count,
    }
}

/// Pause every pending download in `target` until the user says otherwise.
///
/// These pauses are deliberately not scheduler-owned: otherwise the next pass
/// would hand the turn straight back to the batch the user just held.
pub fn plan_hold_batch(items: &[DownloadItem], target: QueueBatchTarget) -> Vec<QueueAction> {
    let by_gid = items
        .iter()
        .map(|item| (item.gid.as_str(), item))
        .collect::<HashMap<_, _>>();
    items
        .iter()
        .filter_map(|item| {
            if effective_batch(&by_gid, item) != target {
                return None;
            }
            match item.status {
                DownloadStatus::Active | DownloadStatus::Waiting => Some(QueueAction::HoldByUser {
                    gid: item.gid.clone(),
                }),
                DownloadStatus::Paused if item.queue_held => Some(QueueAction::KeepPausedByUser {
                    gid: item.gid.clone(),
                }),
                _ => None,
            }
        })
        .collect()
}

/// Give `target` the turn: release its paused downloads and hand every other
/// batch back to the scheduler so it resumes when its own turn arrives.
pub fn plan_start_batch(items: &[DownloadItem], target: QueueBatchTarget) -> Vec<QueueAction> {
    let by_gid = items
        .iter()
        .map(|item| (item.gid.as_str(), item))
        .collect::<HashMap<_, _>>();
    items
        .iter()
        .filter(|item| is_pending(item))
        .filter_map(|item| {
            let own = effective_batch(&by_gid, item);
            match item.status {
                DownloadStatus::Paused if own == target => Some(QueueAction::Release {
                    gid: item.gid.clone(),
                }),
                DownloadStatus::Active | DownloadStatus::Waiting if own != target => {
                    Some(QueueAction::Hold {
                        gid: item.gid.clone(),
                    })
                }
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(gid: &str, status: DownloadStatus, batch: Option<u32>, held: bool) -> DownloadItem {
        DownloadItem {
            gid: gid.into(),
            status,
            name: format!("{gid}.iso"),
            primary_path: None,
            source_uri: None,
            info_hash: None,
            num_seeders: None,
            followed_by: Vec::new(),
            belongs_to: None,
            is_metadata_only: false,
            total_bytes: 100,
            completed_bytes: 10,
            download_speed_bps: 0,
            realtime_download_speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: None,
            error_code: None,
            error_message: None,
            batch,
            queue_held: held,
        }
    }

    #[test]
    fn later_batches_are_held_while_an_earlier_batch_runs() {
        let items = vec![
            item("fast", DownloadStatus::Active, Some(0), false),
            item("queued", DownloadStatus::Waiting, Some(1), false),
            item("last", DownloadStatus::Waiting, None, false),
        ];
        let (active, actions) = plan_batch_policy(&items);

        assert_eq!(active, Some(QueueBatchTarget::Number(0)));
        assert_eq!(
            actions,
            vec![
                QueueAction::Hold {
                    gid: "queued".into()
                },
                QueueAction::Hold { gid: "last".into() },
            ]
        );
    }

    #[test]
    fn same_batch_downloads_together() {
        let items = vec![
            item("one", DownloadStatus::Active, Some(2), false),
            item("two", DownloadStatus::Waiting, Some(2), false),
        ];
        let (active, actions) = plan_batch_policy(&items);

        assert_eq!(active, Some(QueueBatchTarget::Number(2)));
        assert!(actions.is_empty());
    }

    #[test]
    fn held_batch_is_released_when_earlier_batches_finish() {
        let items = vec![
            item("waiting-turn", DownloadStatus::Paused, Some(1), true),
            item("also-held", DownloadStatus::Paused, None, true),
        ];
        let (active, actions) = plan_batch_policy(&items);

        assert_eq!(active, Some(QueueBatchTarget::Number(1)));
        assert_eq!(
            actions,
            vec![QueueAction::Release {
                gid: "waiting-turn".into()
            }]
        );
    }

    #[test]
    fn manual_pauses_are_never_resumed() {
        let items = vec![item("user-paused", DownloadStatus::Paused, Some(0), false)];
        let (active, actions) = plan_batch_policy(&items);

        assert_eq!(active, None);
        assert!(actions.is_empty());
    }

    #[test]
    fn unassigned_batch_waits_for_every_numbered_batch() {
        let items = vec![
            item("magnet", DownloadStatus::Active, None, false),
            item("numbered", DownloadStatus::Waiting, Some(7), false),
        ];
        let (active, actions) = plan_batch_policy(&items);

        assert_eq!(active, Some(QueueBatchTarget::Number(7)));
        assert_eq!(
            actions,
            vec![QueueAction::Hold {
                gid: "magnet".into()
            }]
        );
    }

    #[test]
    fn child_downloads_inherit_their_parent_batch() {
        let mut child = item("child", DownloadStatus::Active, None, false);
        child.belongs_to = Some("parent".into());
        child.batch = None;
        let items = vec![
            item("parent", DownloadStatus::Active, Some(1), false),
            child,
            item("later", DownloadStatus::Waiting, Some(2), false),
        ];
        let (active, actions) = plan_batch_policy(&items);

        assert_eq!(active, Some(QueueBatchTarget::Number(1)));
        assert_eq!(
            actions,
            vec![QueueAction::Hold {
                gid: "later".into()
            }]
        );
    }

    #[test]
    fn summary_counts_batches_lowest_first() {
        let items = vec![
            item("a", DownloadStatus::Active, Some(1), false),
            item("b", DownloadStatus::Waiting, Some(1), false),
            item("c", DownloadStatus::Paused, Some(2), true),
            item("done", DownloadStatus::Complete, Some(0), false),
        ];
        let summary = summarize_queue(2, Some(QueueBatchTarget::Number(1)), &items);

        assert_eq!(summary.slots, 2);
        assert_eq!(summary.pending_count, 3);
        assert_eq!(summary.held_count, 1);
        assert_eq!(summary.batches.len(), 2);
        assert_eq!(summary.batches[0].target, QueueBatchTarget::Number(1));
        assert_eq!(summary.batches[0].running, 1);
        assert_eq!(summary.batches[0].waiting, 1);
        assert_eq!(summary.batches[1].target, QueueBatchTarget::Number(2));
        assert_eq!(summary.batches[1].held, 1);
    }

    #[test]
    fn summary_counts_child_when_its_parent_has_left_the_current_list() {
        let mut child = item("torrent-child", DownloadStatus::Active, Some(4), false);
        child.belongs_to = Some("metadata-parent".into());

        let summary = summarize_queue(2, Some(QueueBatchTarget::Number(4)), &[child]);

        assert_eq!(summary.pending_count, 1);
        assert_eq!(summary.batches.len(), 1);
        assert_eq!(summary.batches[0].target, QueueBatchTarget::Number(4));
        assert_eq!(summary.batches[0].running, 1);
    }

    #[test]
    fn holding_a_batch_keeps_it_paused_past_its_turn() {
        let items = vec![
            item("current", DownloadStatus::Active, Some(0), false),
            item("later", DownloadStatus::Paused, Some(1), true),
        ];
        let held = plan_hold_batch(&items, QueueBatchTarget::Number(0));
        assert_eq!(
            held,
            vec![QueueAction::HoldByUser {
                gid: "current".into()
            }]
        );

        // A user-held batch is not handed the turn; the next one goes instead.
        let after: Vec<DownloadItem> = vec![
            item("current", DownloadStatus::Paused, Some(0), false),
            item("later", DownloadStatus::Paused, Some(1), true),
        ];
        let (active, actions) = plan_batch_policy(&after);
        assert_eq!(active, Some(QueueBatchTarget::Number(1)));
        assert_eq!(
            actions,
            vec![QueueAction::Release {
                gid: "later".into()
            }]
        );
    }

    #[test]
    fn holding_a_future_batch_transfers_its_scheduler_pause_to_the_user() {
        let items = vec![item("later", DownloadStatus::Paused, Some(2), true)];

        assert_eq!(
            plan_hold_batch(&items, QueueBatchTarget::Number(2)),
            vec![QueueAction::KeepPausedByUser {
                gid: "later".into()
            }]
        );
    }

    #[test]
    fn child_inherits_batch_when_parent_is_no_longer_in_snapshot() {
        let mut state = QueueState::default();
        state.set_batch("metadata-parent", Some(7));
        let mut child = item("torrent-child", DownloadStatus::Active, None, false);
        child.belongs_to = Some("metadata-parent".into());

        attach_metadata(&state, std::slice::from_mut(&mut child));

        assert_eq!(child.batch, Some(7));
    }

    #[test]
    fn starting_a_batch_parks_other_batches_for_their_turn() {
        let items = vec![
            item("current", DownloadStatus::Active, Some(0), false),
            item("wanted", DownloadStatus::Paused, Some(2), false),
        ];
        let actions = plan_start_batch(&items, QueueBatchTarget::Number(2));
        assert_eq!(
            actions,
            vec![
                QueueAction::Hold {
                    gid: "current".into()
                },
                QueueAction::Release {
                    gid: "wanted".into()
                },
            ]
        );
    }

    #[test]
    fn queue_entries_round_trip_and_prune() {
        let mut state = QueueState::default();
        state.set_batch("gid-a", Some(3));
        state.set_held("gid-b", true);
        assert_eq!(state.batch("gid-a"), Some(3));
        assert!(state.is_held("gid-b"));
        assert!(!state.is_held("gid-a"));
        state.retain(|gid| gid == "gid-a");
        assert_eq!(state.len(), 1);
        assert!(!state.is_held("gid-b"));
    }
}
