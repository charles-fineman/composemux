//! Whether the Docker daemon is still answering.
//!
//! The supervisor and the service poller both retry indefinitely when the
//! daemon goes away, so a lost connection is recoverable rather than fatal.
//! What was missing is any sign of it: this decides when an outage has lasted
//! long enough to be worth telling the user about.

/// Consecutive failed polls before the daemon is called unreachable.
///
/// Three, so that the note costs two more failures than a single dropped
/// request. That one case is what the debounce exists for: a transient error
/// must never flash a note the next poll immediately takes back.
///
/// What that costs in latency depends on how the daemon fails. Against one
/// that refuses connections outright, each poll returns at once and the round
/// is paced by the poller's own rest of 0.5s to 2s (`MIN_REFRESH` to `REFRESH`
/// in `main`), so the note lands within about six seconds; measured against a
/// cut socket it took 2.7s. Against a daemon that accepts and then never
/// answers, the poll blocks on bollard's own request timeout instead and the
/// note is far later -- see the note on [`ConnectionHealth`].
const FAILURES_BEFORE_UNREACHABLE: u32 = 3;

/// Tracks whether the daemon is still answering the periodic service poll.
///
/// It counts completed polls rather than measuring elapsed time, because a
/// run of failures is unambiguous where a wall-clock window is not: a window
/// would also fire part way through a single slow request that may yet
/// succeed, which is a different thing from an outage.
///
/// The cost of that choice is that a daemon which accepts a connection and
/// then never answers is not noticed until the request itself gives up, which
/// against bollard's default timeout is minutes rather than seconds. That is a
/// real gap and not merely a tradeoff -- #48 tracks bounding the poll so a
/// hung daemon counts as a failed one.
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
        // Strict alternation first, which is the case the name describes: any
        // number of these must never reach the threshold.
        let mut health = ConnectionHealth::default();
        for _ in 0..20 {
            health.record_failure();
            health.record_success();
            assert!(
                !health.is_unreachable(),
                "an answered poll is not an outage"
            );
        }
        // Then a run stopped one short, resumed after a success, which is the
        // near miss an off-by-one would let through.
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

    /// The other tests derive their loop bounds from the constant, so they
    /// hold for whatever it is set to. This one is written out longhand,
    /// because the value itself is a promise about how long the screen may
    /// stay silent, and changing it should have to be deliberate.
    #[test]
    fn the_threshold_is_three_failures_exactly() {
        assert_eq!(FAILURES_BEFORE_UNREACHABLE, 3);
        let mut health = ConnectionHealth::default();
        health.record_failure();
        assert!(!health.is_unreachable(), "one failure");
        health.record_failure();
        assert!(!health.is_unreachable(), "two failures");
        health.record_failure();
        assert!(health.is_unreachable(), "three failures");
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
