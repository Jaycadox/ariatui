use std::time::Instant;

use tracing::debug;

#[derive(Debug)]
pub struct StartupTrace {
    enabled: bool,
    start: Instant,
    last: Instant,
}

impl StartupTrace {
    pub fn new(enabled: bool) -> Self {
        let now = Instant::now();
        Self {
            enabled,
            start: now,
            last: now,
        }
    }

    pub fn mark(&mut self, stage: &str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let delta = now.duration_since(self.last);
        let total = now.duration_since(self.start);
        self.last = now;
        debug!(
            target: "ariatui::startup",
            stage,
            delta_ms = delta.as_millis(),
            total_ms = total.as_millis(),
            "startup stage"
        );
    }
}
