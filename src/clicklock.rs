//! Temporary click lock (tap-to-drag), ported from `tapdrag`.
//!
//! Hold the left button for `hold`; when it is released the virtual button stays
//! down. Every nonzero relative motion pushes the release deadline out by
//! `release`. Pressing the button again freezes the deadline until it is
//! released ("renew"). A deadline expiry emits the synthetic release.

use std::time::{Duration, Instant};

use crate::config::{Settings, CLICK_LOCK_HOLD, CLICK_LOCK_RELEASE};

#[derive(Clone, Debug)]
pub struct ClickLock {
    enabled: bool,
    hold: Duration,
    release: Duration,
    pressed: bool,
    press_at: Option<Instant>,
    locked: bool,
    renewing: bool,
    deadline: Option<Instant>,
}

impl Default for ClickLock {
    fn default() -> Self {
        ClickLock {
            enabled: false,
            hold: CLICK_LOCK_HOLD,
            release: CLICK_LOCK_RELEASE,
            pressed: false,
            press_at: None,
            locked: false,
            renewing: false,
            deadline: None,
        }
    }
}

impl ClickLock {
    pub fn from_settings(s: &Settings) -> Self {
        let mut cl = ClickLock::default();
        cl.apply(s);
        cl
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Update from resolved settings. Returns true if a lock was released
    /// because click lock was disabled.
    pub fn apply(&mut self, s: &Settings) -> bool {
        if !s.click_lock {
            let released = self.locked;
            self.enabled = false;
            self.reset();
            return released;
        }
        if !self.enabled {
            // Newly enabled: drop any stale in-progress state.
            self.reset();
            self.enabled = true;
        }
        self.hold = s.click_lock_hold;
        self.release = s.click_lock_release;
        false
    }

    fn reset(&mut self) {
        self.pressed = false;
        self.press_at = None;
        self.locked = false;
        self.renewing = false;
        self.deadline = None;
    }

    /// Handle a `BTN_LEFT` event. Returns true if it should be forwarded.
    pub fn on_left(&mut self, value: i32, now: Instant) -> bool {
        if !self.enabled {
            return true;
        }
        match value {
            1 => {
                if !self.locked {
                    if !self.pressed {
                        self.pressed = true;
                        self.press_at = Some(now);
                    }
                    return true;
                }
                // Re-press while locked: keep it held, freeze the deadline.
                self.renewing = true;
                self.deadline = None;
                false
            }
            0 => {
                if self.locked {
                    if self.renewing {
                        self.renewing = false;
                        self.deadline = Some(now + self.release);
                    }
                    return false;
                }
                if !self.pressed {
                    return true;
                }
                self.pressed = false;
                let held = self
                    .press_at
                    .take()
                    .map(|t| now.saturating_duration_since(t))
                    .unwrap_or_default();
                if held >= self.hold {
                    self.locked = true;
                    self.deadline = Some(now + self.release);
                    return false;
                }
                true
            }
            // Autorepeat: only forward while not locked.
            _ => !self.locked,
        }
    }

    /// Renew the release deadline on real pointer motion.
    pub fn on_motion(&mut self, now: Instant) {
        if self.enabled && self.locked && !self.renewing {
            self.deadline = Some(now + self.release);
        }
    }

    /// Time until the synthetic release is due, if any.
    pub fn next_timeout(&self, now: Instant) -> Option<Duration> {
        if !self.locked || self.renewing {
            return None;
        }
        self.deadline.map(|d| d.saturating_duration_since(now))
    }

    /// Emit a release if the deadline has passed. Returns true if it did.
    pub fn on_timeout(&mut self, now: Instant) -> bool {
        if !self.enabled || !self.locked || self.renewing {
            return false;
        }
        let Some(deadline) = self.deadline else {
            return false;
        };
        if now < deadline {
            return false;
        }
        self.locked = false;
        self.deadline = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigFile;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn enabled() -> ClickLock {
        let mut s = ConfigFile::default().resolve_global();
        s.click_lock = true;
        ClickLock::from_settings(&s)
    }

    #[test]
    fn short_click_is_forwarded() {
        let base = Instant::now();
        let mut cl = enabled();
        assert!(cl.on_left(1, at(base, 0)));
        assert!(cl.on_left(0, at(base, 100))); // < 180ms
        assert!(!cl.is_locked());
    }

    #[test]
    fn long_hold_locks_and_timeout_releases() {
        let base = Instant::now();
        let mut cl = enabled();
        assert!(cl.on_left(1, at(base, 0)));
        assert!(!cl.on_left(0, at(base, 200))); // swallow, lock
        assert!(cl.is_locked());
        assert_eq!(
            cl.next_timeout(at(base, 200)),
            Some(Duration::from_millis(300))
        );
        assert!(!cl.on_timeout(at(base, 400)));
        assert!(cl.on_timeout(at(base, 500)));
        assert!(!cl.is_locked());
    }

    #[test]
    fn motion_renews_deadline() {
        let base = Instant::now();
        let mut cl = enabled();
        cl.on_left(1, at(base, 0));
        cl.on_left(0, at(base, 200));
        cl.on_motion(at(base, 400));
        assert_eq!(
            cl.next_timeout(at(base, 400)),
            Some(Duration::from_millis(300))
        );
        assert!(cl.on_timeout(at(base, 700)));
        assert!(!cl.is_locked());
    }

    #[test]
    fn repress_freezes_then_renews() {
        let base = Instant::now();
        let mut cl = enabled();
        cl.on_left(1, at(base, 0));
        cl.on_left(0, at(base, 200)); // locked
        assert!(!cl.on_left(1, at(base, 300))); // renewing
        assert_eq!(cl.next_timeout(at(base, 900)), None);
        assert!(!cl.on_left(0, at(base, 1000)));
        assert_eq!(
            cl.next_timeout(at(base, 1000)),
            Some(Duration::from_millis(300))
        );
    }

    #[test]
    fn disable_releases_lock() {
        let base = Instant::now();
        let mut cl = enabled();
        cl.on_left(1, at(base, 0));
        cl.on_left(0, at(base, 200));
        assert!(cl.is_locked());
        let mut s = ConfigFile::default().resolve_global();
        s.click_lock = false;
        assert!(cl.apply(&s));
        assert!(!cl.is_locked());
    }
}
