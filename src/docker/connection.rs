//! Whether the Docker daemon is still answering.
//!
//! The supervisor and the service poller both retry indefinitely when the
//! daemon goes away, so a lost connection is recoverable rather than fatal.
//! What was missing is any sign of it: this decides when an outage has lasted
//! long enough to be worth telling the user about.

/// Consecutive failed polls before the daemon is called unreachable.
///
/// The service poller rests between 0.5s and 2s per round (`MIN_REFRESH` to
/// `REFRESH` in `main`), so three consecutive failures put the note on screen
/// somewhere between one and six seconds after the daemon stops answering.
/// That is quick enough to answer "is this thing stuck?" while still costing
/// two more failures than a single dropped request, which is the case the
/// debounce exists for: one transient error must never flash a note that the
/// next poll immediately takes back.
const FAILURES_BEFORE_UNREACHABLE: u32 = 3;

/// Consecutive failures of the periodic service poll.
///
/// Deliberately counts polls rather than measuring elapsed time. The poll is
/// the only regular round trip we make, so a run of failures is exactly the
/// evidence that the daemon is not answering — where a wall-clock window would
/// also fire during a long single request that is still in flight and may yet
/// succeed.
#[derive(Debug, Default)]
pub struct ConnectionHealth {
    /// Failed polls since the last success. Saturates rather than wrapping, so
    /// a very long outage cannot fall back to "reachable".
    consecutive_failures: u32,
}

impl ConnectionHealth {
    /// Records a poll the daemon answered, ending any outage.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Records a poll the daemon did not answer.
    pub fn record_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
    }

    /// Whether the outage has run long enough to show.
    pub fn is_unreachable(&self) -> bool {
        self.consecutive_failures >= FAILURES_BEFORE_UNREACHABLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_that_has_not_failed_is_reachable() {
        assert!(!ConnectionHealth::default().is_unreachable());
    }

    /// The whole point of the debounce: a single dropped request is not an
    /// outage, and flashing a note that the next poll takes back would be
    /// worse than saying nothing.
    #[test]
    fn a_single_failed_poll_is_not_an_outage() {
        let mut health = ConnectionHealth::default();
        health.record_failure();
        assert!(!health.is_unreachable());
    }

    #[test]
    fn failures_short_of_the_threshold_stay_silent() {
        let mut health = ConnectionHealth::default();
        for _ in 1..FAILURES_BEFORE_UNREACHABLE {
            health.record_failure();
            assert!(
                !health.is_unreachable(),
                "reported an outage before {FAILURES_BEFORE_UNREACHABLE} failures"
            );
        }
    }

    #[test]
    fn the_threshold_run_of_failures_reports_an_outage() {
        let mut health = ConnectionHealth::default();
        for _ in 0..FAILURES_BEFORE_UNREACHABLE {
            health.record_failure();
        }
        assert!(health.is_unreachable());
    }

    /// Failures have to be consecutive. A daemon that answers every other poll
    /// is degraded, not gone, and the note would otherwise blink on and off.
    #[test]
    fn a_success_between_failures_starts_the_count_again() {
        let mut health = ConnectionHealth::default();
        for _ in 1..FAILURES_BEFORE_UNREACHABLE {
            health.record_failure();
        }
        health.record_success();
        for _ in 1..FAILURES_BEFORE_UNREACHABLE {
            health.record_failure();
        }
        assert!(
            !health.is_unreachable(),
            "the run was broken by a success, so it must start again"
        );
    }

    #[test]
    fn one_answered_poll_ends_an_outage() {
        let mut health = ConnectionHealth::default();
        for _ in 0..FAILURES_BEFORE_UNREACHABLE + 5 {
            health.record_failure();
        }
        assert!(health.is_unreachable());
        health.record_success();
        assert!(!health.is_unreachable(), "recovery must clear the note");
    }

    /// A daemon that is down for days must not wrap the counter back under the
    /// threshold and quietly declare itself reachable again.
    #[test]
    fn a_very_long_outage_does_not_wrap_back_to_reachable() {
        let mut health = ConnectionHealth {
            consecutive_failures: u32::MAX,
        };
        health.record_failure();
        assert!(health.is_unreachable());
    }
}
