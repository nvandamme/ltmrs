//! Idle-exit behavior (design §7.2).

/// Tracks connection activity to decide when the daemon may idle-exit.
pub struct IdleExitTracker {
    active: usize,
    last_activity: u64,
}

impl IdleExitTracker {
    pub fn new(now_millis: u64) -> Self {
        Self {
            active: 0,
            last_activity: now_millis,
        }
    }

    pub fn note_connect(&mut self, now_millis: u64) {
        self.active += 1;
        self.last_activity = now_millis;
    }

    pub fn note_disconnect(&mut self, now_millis: u64) {
        self.active = self.active.saturating_sub(1);
        self.last_activity = now_millis;
    }

    pub fn note_activity(&mut self, now_millis: u64) {
        self.last_activity = now_millis;
    }

    pub fn active_connections(&self) -> usize {
        self.active
    }

    pub fn should_exit(&self, now_millis: u64, idle_timeout_millis: u64) -> bool {
        self.active == 0 && now_millis.saturating_sub(self.last_activity) >= idle_timeout_millis
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exits_when_idle_after_timeout() {
        let t = IdleExitTracker::new(0);
        assert!(!t.should_exit(100, 1000));
        assert!(t.should_exit(1100, 1000));
    }

    #[test]
    fn does_not_exit_while_connections_active() {
        let mut t = IdleExitTracker::new(0);
        t.note_connect(100);
        assert!(!t.should_exit(5000, 1000));
    }

    #[test]
    fn reconnect_resets_idle_window() {
        let mut t = IdleExitTracker::new(0);
        t.note_connect(100);
        t.note_disconnect(200);
        assert!(!t.should_exit(1100, 1000));
        assert!(t.should_exit(1200, 1000));
        t.note_connect(1300);
        assert!(!t.should_exit(2300, 1000));
        t.note_disconnect(2400);
        assert!(!t.should_exit(3300, 1000));
        assert!(t.should_exit(3400, 1000));
    }

    #[test]
    fn activity_resets_idle_window() {
        let mut t = IdleExitTracker::new(0);
        t.note_connect(0);
        t.note_activity(5000);
        t.note_disconnect(5100);
        assert!(!t.should_exit(6000, 1000));
        assert!(t.should_exit(6100, 1000));
    }
}
