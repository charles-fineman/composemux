//! Whether the Docker daemon is still answering.
//!
//! The supervisor and the service poller both retry indefinitely when the
//! daemon goes away, so a lost connection is recoverable rather than fatal.
//! What was missing is any sign of it: this decides when an outage has lasted
//! long enough to be worth telling the user about.

/// Consecutive failed rounds before the daemon is reported as lost.
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
/// answers, each round instead costs `POLL_TIMEOUT` in `main`, so the note
/// lands three of those in -- see that constant's own note for how it was
/// picked.
const FAILURES_BEFORE_OUTAGE: u32 = 3;

/// Why the daemon is not being heard from.
///
/// Worth telling apart because the user acts on them differently: a socket
/// that refuses is a daemon to start, where one that takes the request and
/// then goes quiet is a daemon that is very likely running and wedged, and
/// restarting the tool will not help. Calling the second "unreachable" would
/// send the user to check something that is probably fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outage {
    /// The request came back a failure, whatever the cause: a socket that was
    /// refused or cut, and also a daemon that answered with an error of its
    /// own. The second is not literally unreachable, which is a rough edge the
    /// note inherits rather than one this introduced.
    Unreachable,
    /// The request has been outstanding longer than the poller is willing to
    /// wait for it, without either answering or failing. The daemon may be
    /// wedged, or merely slower than anything we would call healthy.
    NotAnswering,
}

/// Tracks whether the daemon is still answering the periodic service poll.
///
/// A round ends either with the daemon's answer or with the poller's own
/// bound on how long it will wait for one, so a request that is taken and
/// never answered ends rounds too and is reported like any other outage.
/// Waiting is all that is given up on: the request stays in flight, so a poll
/// that is merely slow still delivers its services and clears the note.
///
/// Counting those rounds is what buys the headroom. A single wall-clock window
/// would call an outage the first time one request ran long, which is a
/// different thing from a daemon that has gone away; requiring a run of them
/// means the same request has to overrun three times over before anyone is
/// told, and any answer along the way resets it.
#[derive(Debug, Default)]
pub struct ConnectionHealth {
    /// Failed rounds since the last success. Saturates rather than wrapping,
    /// so a very long outage cannot fall back to "reachable".
    consecutive_failures: u32,
    /// How the most recent round failed, which is what the note says once the
    /// run is long enough to show one. The most recent rather than the first,
    /// because a daemon that refused and has since started hanging is hanging
    /// now, and that is what the user is looking at.
    last_failure: Option<Outage>,
}

impl ConnectionHealth {
    /// Records a poll the daemon answered, ending any outage.
    ///
    /// Clearing the kind as well is not observable through [`outage`], which
    /// cannot report one until three further failures have each overwritten
    /// it. It is here so the field means what it says it means: after this,
    /// there is no most recent failure to name.
    ///
    /// [`outage`]: Self::outage
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.last_failure = None;
    }

    /// Records a round that ended without an answer, and how.
    pub fn record_failure(&mut self, kind: Outage) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_failure = Some(kind);
    }

    /// The outage to report, once the run has been going long enough to show.
    pub fn outage(&self) -> Option<Outage> {
        if self.consecutive_failures >= FAILURES_BEFORE_OUTAGE {
            self.last_failure
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_that_has_not_failed_is_reachable() {
        assert!(ConnectionHealth::default().outage().is_none());
    }

    /// The whole point of the debounce: a single dropped request is not an
    /// outage, and flashing a note that the next poll takes back would be
    /// worse than saying nothing.
    #[test]
    fn a_single_failed_poll_is_not_an_outage() {
        let mut health = ConnectionHealth::default();
        health.record_failure(Outage::Unreachable);
        assert!(health.outage().is_none());
    }

    #[test]
    fn failures_short_of_the_threshold_stay_silent() {
        let mut health = ConnectionHealth::default();
        for _ in 1..FAILURES_BEFORE_OUTAGE {
            health.record_failure(Outage::Unreachable);
            assert!(
                health.outage().is_none(),
                "reported an outage before {FAILURES_BEFORE_OUTAGE} failures"
            );
        }
    }

    #[test]
    fn the_threshold_run_of_failures_reports_an_outage() {
        let mut health = ConnectionHealth::default();
        for _ in 0..FAILURES_BEFORE_OUTAGE {
            health.record_failure(Outage::Unreachable);
        }
        assert_eq!(health.outage(), Some(Outage::Unreachable));
    }

    /// The note has to name the failure the user is living with now. A daemon
    /// that refused while it was restarting and has since come up wedged is
    /// hanging now, and a note still saying "unreachable" would send the user
    /// to start a daemon that is already running.
    #[test]
    fn the_reported_outage_is_the_most_recent_kind() {
        let mut health = ConnectionHealth::default();
        health.record_failure(Outage::Unreachable);
        health.record_failure(Outage::Unreachable);
        health.record_failure(Outage::NotAnswering);
        assert_eq!(health.outage(), Some(Outage::NotAnswering));
    }

    /// Failures have to be consecutive. A daemon that answers every other poll
    /// is degraded, not gone, and the note would otherwise blink on and off.
    #[test]
    fn a_success_between_failures_starts_the_count_again() {
        // Strict alternation first, which is the case the name describes: any
        // number of these must never reach the threshold.
        let mut health = ConnectionHealth::default();
        for _ in 0..20 {
            health.record_failure(Outage::Unreachable);
            // Sampled here rather than after the success, because here is
            // where the production code looks: `poll_outcome` records a
            // failure and reads this in the same breath, so this is the state
            // that decides whether the note flashes. After a success the count
            // is trivially zero and the assertion would prove nothing.
            assert!(
                health.outage().is_none(),
                "an answered poll is not an outage"
            );
            health.record_success();
        }
        // Then a run stopped one short, resumed after a success, which is the
        // near miss an off-by-one would let through.
        for _ in 1..FAILURES_BEFORE_OUTAGE {
            health.record_failure(Outage::Unreachable);
        }
        health.record_success();
        for _ in 1..FAILURES_BEFORE_OUTAGE {
            health.record_failure(Outage::Unreachable);
        }
        assert!(
            health.outage().is_none(),
            "the run was broken by a success, so it must start again"
        );
    }

    /// The other tests derive their loop bounds from the constant, so they
    /// hold for whatever it is set to. This one is written out longhand,
    /// because the value itself is a promise about how long the screen may
    /// stay silent, and changing it should have to be deliberate.
    #[test]
    fn the_threshold_is_three_failures_exactly() {
        assert_eq!(FAILURES_BEFORE_OUTAGE, 3);
        let mut health = ConnectionHealth::default();
        health.record_failure(Outage::Unreachable);
        assert!(health.outage().is_none(), "one failure");
        health.record_failure(Outage::Unreachable);
        assert!(health.outage().is_none(), "two failures");
        health.record_failure(Outage::Unreachable);
        assert!(health.outage().is_some(), "three failures");
    }

    #[test]
    fn one_answered_poll_ends_an_outage() {
        let mut health = ConnectionHealth::default();
        for _ in 0..FAILURES_BEFORE_OUTAGE + 5 {
            health.record_failure(Outage::Unreachable);
        }
        assert!(health.outage().is_some());
        health.record_success();
        assert!(health.outage().is_none(), "recovery must clear the note");
    }

    /// A daemon that is down for days must not wrap the counter back under the
    /// threshold and quietly declare itself reachable again.
    #[test]
    fn a_very_long_outage_does_not_wrap_back_to_reachable() {
        let mut health = ConnectionHealth {
            consecutive_failures: u32::MAX,
            last_failure: Some(Outage::Unreachable),
        };
        health.record_failure(Outage::Unreachable);
        assert!(health.outage().is_some());
    }
}
