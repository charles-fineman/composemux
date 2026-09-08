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

/// Why the service poll is not coming back with services.
///
/// Worth telling apart because the user acts on them differently. A socket
/// that refuses is a daemon to start. One that takes the request and then
/// goes quiet is a daemon that is very likely running and wedged, and
/// restarting the tool will not help. One that answers with an error is
/// running, reachable and talking to us, and what has failed is the request
/// -- most often because the socket will not have us. Calling either of the
/// last two "unreachable" sends the user to start something already started,
/// which leaves them with no next step at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outage {
    /// Nothing was reached: a socket that refused the connection, was cut, or
    /// is not there at all. This is the one where "start Docker" is the
    /// remedy.
    Unreachable,
    /// The request has been outstanding longer than the poller is willing to
    /// wait for it, without either answering or failing. The daemon may be
    /// wedged, or merely slower than anything we would call healthy.
    NotAnswering,
    /// The request was refused rather than lost: the daemon answered with an
    /// error of its own, or the socket would not let us open it.
    ///
    /// Only the first of those establishes that a daemon is running -- a
    /// refusal by the socket's permissions happens in the kernel, before a
    /// byte is exchanged, and says only that the socket node is there. What
    /// the two share is the remedy they rule out: neither is fixed by
    /// starting Docker, which is where both of the other notes send the user.
    /// The note says only that the request was rejected, for that reason.
    Rejected,
}

impl Outage {
    /// Which outage a failed Docker request amounts to.
    ///
    /// Read off typed error values rather than message text, which is the
    /// daemon's to change. Two things say the far end is there and talking:
    ///
    /// * [`bollard::errors::Error::DockerResponseServerError`], which carries
    ///   a status code, and a status code at all means the daemon framed a
    ///   reply. That covers both a 500 and an API version it will not serve,
    ///   which it declines with a 400 rather than by dropping the connection.
    /// * a [`std::io::Error`] of kind
    ///   [`PermissionDenied`](std::io::ErrorKind::PermissionDenied), which is
    ///   the socket refusing to be opened -- the "user is not in the docker
    ///   group" case. A path we cannot traverse lands here too, and is
    ///   welcome to: the remedy is the same one, and it is not "start
    ///   Docker". So does the case this gets wrong -- `connect(2)` also gives
    ///   `EACCES` when a firewall rule or a MAC policy blocks the connection,
    ///   which is genuinely nothing reached. That needs a `tcp://` or `ssh://`
    ///   host to reach at all, and the error carries nothing to tell it from
    ///   a socket's own permissions, so it is left misfiled rather than
    ///   guessed at.
    ///
    /// Everything else stays [`Unreachable`](Self::Unreachable), which is
    /// where it has always been.
    ///
    /// Walked over the whole of [`anyhow::Error::chain`] rather than
    /// downcast at the top, because neither of those is at the top. Every
    /// caller wraps the failure in context first -- `list_services` adds
    /// "could not list containers" -- and the permission case is deeper
    /// still. Against a socket at mode 000, bollard 0.21.1 gives:
    ///
    /// ```text
    /// 0: could not list containers
    /// 1: Error in the hyper legacy client: client error (Connect)
    /// 2: client error (Connect)
    /// 3: Permission denied (os error 13)
    /// ```
    ///
    /// so the kind is three deep, under a hyper error, and the bollard
    /// variant holding it is `HyperLegacyError` rather than the `IOError` its
    /// name suggests. Matching the `io::Error` wherever it is found is what
    /// makes this independent of which transport bollard chose. Naming the
    /// variant instead would also mean naming hyper's error type to reach the
    /// kind, and that one cannot be built from outside hyper -- which is why
    /// the test for this has to reproduce the nesting rather than borrow it.
    ///
    /// `IOError` is then checked on the bollard value as well, and not
    /// because two ways of spelling it is tidy: it is declared
    /// `#[error(transparent)]`, so thiserror's `source` forwards past the
    /// `io::Error` to *its* source, which is `None`. An `io::Error` held that
    /// way never appears in the chain at all, and the chain walk alone would
    /// miss it.
    pub fn classify(err: &anyhow::Error) -> Self {
        for cause in err.chain() {
            if let Some(docker) = cause.downcast_ref::<bollard::errors::Error>() {
                match docker {
                    bollard::errors::Error::DockerResponseServerError { .. } => {
                        return Self::Rejected
                    }
                    bollard::errors::Error::IOError { err }
                        if err.kind() == std::io::ErrorKind::PermissionDenied =>
                    {
                        return Self::Rejected
                    }
                    _ => {}
                }
            }
            if cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
            {
                return Self::Rejected;
            }
        }
        // Not a failure we can say anything more about than the single kind
        // always did, so it keeps reading the way it always has.
        Self::Unreachable
    }
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
    use anyhow::Context;

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

    /// A 500 from the daemon: it is running, it took the request, and it
    /// answered. #64's complaint is that this was called unreachable, which
    /// sends the user to start something already started.
    ///
    /// Told by variant, not by message: `DockerResponseServerError` carries a
    /// status code, which is the whole of what "the daemon answered" means.
    #[test]
    fn a_daemon_that_answers_with_an_error_is_not_unreachable() {
        let err = anyhow::Error::from(bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "server error".to_string(),
        });
        assert_eq!(Outage::classify(&err), Outage::Rejected);
    }

    /// The API version the daemon will not serve, which it declines with a
    /// 400 rather than by dropping the connection. Same variant, so this is
    /// really the same claim as the case above -- written down because #64
    /// names it separately and a future classifier keyed on the status code
    /// would quietly stop covering it.
    #[test]
    fn an_api_version_the_daemon_refuses_is_not_unreachable() {
        let err = anyhow::Error::from(bollard::errors::Error::DockerResponseServerError {
            status_code: 400,
            message: "client version 1.99 is too new".to_string(),
        });
        assert_eq!(Outage::classify(&err), Outage::Rejected);
    }

    /// How a refused connection really reaches us, which is not the shape the
    /// variant names suggest: bollard hands back `HyperLegacyError`, and the
    /// `io::Error` is two further sources down. Named for hyper's own message
    /// because that is what it stands in for; hyper's error cannot be built
    /// from outside hyper, so the nesting is reproduced rather than borrowed.
    /// The real chain is quoted on [`Outage::classify`], measured against
    /// bollard 0.21.1.
    #[derive(Debug)]
    struct Connect(std::io::Error);

    impl std::fmt::Display for Connect {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "client error (Connect)")
        }
    }

    impl std::error::Error for Connect {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// The case #64 calls the one most worth getting right: the socket is
    /// there and will not have us, which is "your user is not in the docker
    /// group" and not "docker is not running".
    ///
    /// Both shapes it can arrive in. The nested one is what a real daemon
    /// produces, and is the reason this walks the chain rather than
    /// downcasting the outermost error; the flat one is the variant a reader
    /// would expect from the name, and would be a silent regression to stop
    /// covering.
    #[test]
    fn a_socket_that_denies_permission_is_not_unreachable() {
        let nested = anyhow::Error::new(Connect(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )))
        .context("could not list containers");
        assert_eq!(Outage::classify(&nested), Outage::Rejected, "nested");

        let flat = anyhow::Error::from(bollard::errors::Error::IOError {
            err: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        });
        assert_eq!(Outage::classify(&flat), Outage::Rejected, "flat");
    }

    /// The other `io::ErrorKind`s, which really are nothing reached. Without
    /// this the permission case above could be passed by calling every
    /// connect failure a rejection, which would tell a user whose daemon is
    /// stopped that it is running -- the exact untruth #64 is about, pointing
    /// the other way.
    #[test]
    fn a_socket_that_is_not_there_is_still_unreachable() {
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::BrokenPipe,
        ] {
            let nested = anyhow::Error::new(Connect(std::io::Error::from(kind)))
                .context("could not list containers");
            assert_eq!(
                Outage::classify(&nested),
                Outage::Unreachable,
                "{kind:?} is a daemon that was never reached"
            );
        }
        // The one bollard raises before a connection is even attempted, when
        // `DOCKER_HOST` names a socket that is not on disk at all.
        let missing = anyhow::Error::from(bollard::errors::Error::SocketNotFoundError(
            "/var/run/docker.sock".to_string(),
        ));
        assert_eq!(Outage::classify(&missing), Outage::Unreachable);
    }

    /// Every caller wraps the failure in context before it reaches here --
    /// `list_services` adds "could not list containers", and the poller may
    /// add more -- so classifying only the outermost error would see an
    /// `anyhow` message and nothing else, and quietly call every rejection
    /// unreachable. Two layers, because one is what a single `downcast_ref`
    /// happens to survive.
    #[test]
    fn the_kind_is_found_underneath_the_context_the_callers_add() {
        let err = Err::<(), _>(bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "server error".to_string(),
        })
        .context("could not list containers")
        .context("service poll failed")
        .unwrap_err();
        assert_eq!(Outage::classify(&err), Outage::Rejected);
    }

    /// A failure that is not bollard's at all keeps reading the way it always
    /// has, rather than being guessed at.
    #[test]
    fn a_failure_that_is_not_the_daemons_stays_unreachable() {
        let err = anyhow::anyhow!("something else went wrong");
        assert_eq!(Outage::classify(&err), Outage::Unreachable);
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
