use chrono::{DateTime, Local, Timelike};
use color_eyre::eyre::Result;

use crate::state::{ManualOrScheduled, PersistedState};

#[derive(Debug, Clone)]
pub struct ResolvedSchedule {
    pub current_hour: u8,
    pub effective_limit_bps: Option<u64>,
    pub next_change_at_local: String,
    pub schedule_limits_bps: [Option<u64>; 24],
}

pub fn resolve(now: DateTime<Local>, state: &PersistedState) -> Result<ResolvedSchedule> {
    let current_hour = now.hour() as u8;
    let schedule_limits_bps = state.schedule_bps()?;
    let effective_limit_bps = match state.mode {
        ManualOrScheduled::Manual => state.manual_limit_bps()?,
        ManualOrScheduled::Scheduled => schedule_limits_bps[current_hour as usize],
    };
    let next_hour = (current_hour + 1) % 24;
    let next_change_at_local = format!("{next_hour:02}:00");

    Ok(ResolvedSchedule {
        current_hour,
        effective_limit_bps,
        next_change_at_local,
        schedule_limits_bps,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::*;
    use crate::state::{ManualOrScheduled, PersistedState};

    #[test]
    fn resolves_manual_and_hour_slot() {
        let state = PersistedState {
            mode: ManualOrScheduled::Scheduled,
            manual_limit: "1M".into(),
            remembered_cancel_behavior: Default::default(),
            schedule: (0..24).map(|i| format!("{}K", i + 1)).collect(),
            default_download_dir: "~/Downloads".into(),
            download_rules: vec![crate::routing::DownloadRoutingRule {
                pattern: "*".into(),
                directory: "~/Downloads".into(),
            }],
        };
        let now = Local.with_ymd_and_hms(2026, 4, 7, 10, 30, 0).unwrap();
        let resolved = resolve(now, &state).expect("resolve");
        assert_eq!(resolved.current_hour, 10);
        assert_eq!(resolved.effective_limit_bps, Some(11 * 1024));
        assert_eq!(resolved.next_change_at_local, "11:00");
    }
}
