//! An nx-style TUI for Docker Compose logs.
//!
//! Attaches to a compose project that is already running and multiplexes its
//! services' logs, deliberately mirroring the Nx terminal UI so that muscle
//! memory transfers. It is read-only: it never starts, stops or execs
//! anything.

// Two lints, because one of them is not enough on its own:
// `missing_docs` covers public items and `missing_docs_in_private_items`
// covers the rest, which in a binary crate is nearly everything. The
// per-file allows mark what predates the rule -- deleting one is how the
// backlog gets paid down, and adding one is not on.
//
// Keep an allow off a `mod.rs`: an inner attribute there covers the whole
// subtree, so it would exempt every file under it, including ones that do
// not exist yet.
#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

/// Configuration file and flag handling.
mod config;
/// Talking to the Docker daemon: discovery, log streams and events.
mod docker;
/// Plain streaming for when stdout is not a terminal.
mod fallback;
/// Services and their log buffers.
mod model;
/// Working out which compose project to attach to.
mod project;
/// The terminal UI.
mod tui;

use anyhow::{bail, Context, Result};
use clap::Parser;
use config::Config;
use crossterm::event::{Event, EventStream, KeyEventKind};
use docker::{ConnectionHealth, DockerClient, LogSupervisor, Outage, ServiceSource, SourceEvent};
use futures::{Stream, StreamExt};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;
use tui::app::{Action, App, ExitReason, ServiceKey};

// Only SIGINT has a windows counterpart (ctrl+c), so the other two would be
// dead code there -- and CI runs clippy at `-D warnings` on windows too.
/// Signal numbers, so an exit status can be reported as `128 + signo` the
/// way a shell would.
#[cfg(unix)]
const SIGHUP: i32 = 1;
/// See the note on the signal constants above.
const SIGINT: i32 = 2;
/// See the note on the signal constants above.
#[cfg(unix)]
const SIGTERM: i32 = 15;

/// Animation tick. Also paces the auto-exit countdown.
const TICK: Duration = Duration::from_millis(100);
/// How often the service list is re-read for status and uptime.
const REFRESH: Duration = Duration::from_secs(2);
/// Floor on how often a refresh may run, so a burst of container events cannot
/// turn the poller into a hot loop against the daemon.
const MIN_REFRESH: Duration = Duration::from_millis(500);
/// How long one round of the service poll will wait for the daemon before
/// counting the round a failure.
///
/// This bounds the *wait*, not the request: an overrun leaves the poll in
/// flight and goes back to waiting on it, so a daemon that is merely slow
/// still delivers its services and clears the note when it finally answers.
/// Nothing a poll would have returned is thrown away, which is what makes a
/// bound this side of pathological safe to pick at all.
///
/// Ten seconds, measured. `list_services` costs one list plus one inspect per
/// container at `INSPECT_CONCURRENCY` 8, so its honest worst case is a large
/// project across a slow link. Against a 150-container project (median of ten
/// polls): 2.1s on a local socket, 3.1s across a 50ms round trip, 6.7s across
/// 200ms, worst single sample 7.1s. A 60-container project cost 0.8s, 1.4s and
/// 3.0s. So ten seconds is already longer than any honest poll observed --
/// and one overrun says nothing on its own, since `FAILURES_BEFORE_OUTAGE`
/// wants three. So a daemon that goes quiet while it is being polled is not
/// called out until one request has been outstanding for thirty seconds --
/// fifteen missed refreshes, and four times the slowest honest poll measured.
/// (A hang that follows failures already on the count is called out sooner,
/// which is right: the daemon has been failing that whole time.)
///
/// Where those measurements stop is worth saying, since they are the argument:
/// 150 containers across a 200ms round trip. Cost grows with containers times
/// round trip, so a project several times larger again over a slow link could
/// cross thirty seconds honestly. What that costs is bounded and clears
/// itself -- the request is still in flight, so the services arrive and take
/// the note back down with them -- which is why the bound is a constant here
/// rather than something to configure.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);
/// Most log messages folded into one redraw. Without a cap, a service logging
/// faster than we can render would keep the drain loop from ever returning, and
/// the UI would stop responding to keys.
const MAX_DRAIN_PER_FRAME: usize = 512;

/// What one round of the service poll has to tell the UI.
///
/// A poll that fails without the daemon having failed often enough to count as
/// an outage yields nothing at all: the app keeps the statuses it has, exactly
/// as it did before, and a single dropped request never reaches the screen.
#[derive(Debug)]
enum Poll {
    /// The daemon answered, with the project's services as it sees them.
    Services(Vec<model::Service>),
    /// The daemon has been failing to answer for long enough to say so, and
    /// how it is failing, which is not the same question.
    Lost(Outage),
}

/// Command-line arguments, which override anything in the config file.
#[derive(Parser, Debug)]
#[command(name = "composemux", version, about, long_about = None)]
struct Args {
    /// Compose project to attach to. Defaults to $COMPOSE_PROJECT_NAME, else
    /// the current directory's name.
    #[arg(short, long)]
    project: Option<String>,

    /// Path to a config file. Defaults to the nearest .composemux.yaml.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Service to pin to an output pane at startup. Repeatable, max two.
    #[arg(long = "pin", value_name = "SERVICE")]
    pin: Vec<String>,

    /// Lines of history to load per service before following.
    #[arg(long)]
    tail: Option<usize>,

    /// Rows of output retained per service. Costs roughly 7 MB per service per
    /// 1000 rows, and sets how long a scrolled-up pane holds its position.
    #[arg(long)]
    scrollback: Option<usize>,

    /// Stream plain prefixed lines instead of the full-screen UI.
    #[arg(long)]
    no_tui: bool,
}

fn main() -> Result<()> {
    let code = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())?;
    std::process::exit(code);
}

/// Everything after argument parsing, returning the process exit status.
async fn run() -> Result<i32> {
    let args = Args::parse();
    let mut cfg = Config::load(args.config.as_deref())?;
    if !args.pin.is_empty() {
        cfg.pinned = args.pin.clone();
    }
    if let Some(tail) = args.tail {
        cfg.tail = tail;
    }
    if let Some(scrollback) = args.scrollback {
        cfg.scrollback = scrollback;
    }

    let project = args
        .project
        .or_else(|| cfg.project.clone())
        .or_else(project::detect)
        .context("could not determine a compose project name; pass --project")?;

    let cancel = CancellationToken::new();
    // -1 until a signal arrives, so a cancellation from any other source keeps
    // its own exit reason.
    let signal_exit = Arc::new(AtomicI32::new(-1));
    // Installed before the first call to the daemon. Startup is not instant --
    // it opens a connection, negotiates an API version and lists containers --
    // and a daemon that is starting up or wedged can stall all three. Handling
    // signals only after that left the slowest part of the program running
    // under the default disposition.
    install_signal_handlers(cancel.clone(), signal_exit.clone())?;

    // Since tokio's handlers *replace* that default disposition, everything
    // from here on has to be cancellable, or a signal would leave the process
    // stalled against an unresponsive daemon with nothing left to kill it.
    let startup = async {
        let client = DockerClient::connect().await?;
        let services = client.list_services(&project).await?;
        if services.is_empty() {
            let available = client.list_projects().await?;
            if available.is_empty() {
                bail!("no compose projects are running");
            }
            bail!(
                "no services found for compose project '{project}'\navailable projects: {}",
                available.join(", ")
            );
        }
        Ok::<DockerClient, anyhow::Error>(client)
    };
    let client = tokio::select! {
        // Biased, so a signal that lands as startup is failing still reports
        // the signal. Under the default random poll order the startup branch
        // could win when both are ready, and `result?` would return the
        // startup error instead of the status the supervisor is waiting for.
        biased;
        _ = cancel.cancelled() => return Ok(exit_status(&signal_exit)),
        result = startup => result?,
    };

    // A full-screen UI is useless when output is piped, and would write escape
    // sequences into whatever is capturing it.
    if args.no_tui || !std::io::stdout().is_terminal() {
        fallback::run(&client, &project, cfg.tail, cancel).await?;
        return Ok(exit_status(&signal_exit));
    }

    run_tui(client, project, cfg, cancel, signal_exit).await
}

/// The full-screen path: draws, and owns the event loop until it exits.
async fn run_tui(
    client: DockerClient,
    project: String,
    cfg: Config,
    cancel: CancellationToken,
    signal_exit: Arc<AtomicI32>,
) -> Result<i32> {
    let client = Arc::new(client);
    let mut app = App::new(&project, &cfg);

    let (log_tx, mut log_rx) = mpsc::channel::<SourceEvent>(4096);
    let supervisor = LogSupervisor::new(&client, &project, cfg.tail, log_tx);
    let supervisor_cancel = cancel.clone();
    spawn_supervised(cancel.clone(), async move {
        supervisor.run(supervisor_cancel).await
    });

    // Service status is polled rather than derived from events, so uptime and
    // health stay current even when nothing is happening.
    let (svc_tx, mut svc_rx) = mpsc::channel::<Poll>(4);
    let refresh = Arc::new(Notify::new());
    spawn_refresher(
        client.clone(),
        project.clone(),
        cfg.clone(),
        svc_tx,
        refresh.clone(),
        cancel.clone(),
    );

    tui::terminal::install_panic_hook();
    let mut terminal = tui::terminal::setup()?;
    // Constructed here rather than inside the loop: crossterm's reader is a
    // process-wide singleton tied to the controlling terminal, and owning it
    // is what a test cannot do.
    let mut events = EventStream::new();
    let result = event_loop(
        &mut terminal,
        &mut events,
        &mut app,
        &mut log_rx,
        &mut svc_rx,
        &refresh,
        &cancel,
    )
    .await;
    tui::terminal::restore()?;

    let exit = result?;
    // A signal outranks whatever the loop reported, so the status reflects how
    // the process was actually asked to stop.
    Ok(match signal_exit.load(Ordering::SeqCst) {
        signo if signo > 0 => ExitReason::Signal(signo).code(),
        _ => exit.code(),
    })
}

/// Wakes the render loop on a timer, so the throbber and uptimes advance
/// even when no logs are arriving.
fn spawn_refresher<S: ServiceSource>(
    source: Arc<S>,
    project: String,
    cfg: Config,
    tx: mpsc::Sender<Poll>,
    refresh: Arc<Notify>,
    cancel: CancellationToken,
) {
    let watchdog = cancel.clone();
    spawn_supervised(
        watchdog,
        refresh_loop(source, project, cfg, tx, refresh, cancel),
    );
}

/// The body of the refresher, separated from the spawn so a test can drive it.
///
/// Generic over [`ServiceSource`] for the same reason: the two cases worth
/// pinning here are a daemon that fails and one that never answers, and
/// neither can be asked of a real daemon.
async fn refresh_loop<S: ServiceSource>(
    source: Arc<S>,
    project: String,
    cfg: Config,
    tx: mpsc::Sender<Poll>,
    refresh: Arc<Notify>,
    cancel: CancellationToken,
) {
    // The poll is the only round trip we make on a fixed cadence, which is
    // what makes it the place to notice the daemon going away.
    let mut health = ConnectionHealth::default();
    // Held across rounds rather than rebuilt each time, so an overrun gives up
    // on waiting without giving up on the request. Cancelling it instead would
    // mean a daemon that answers in eleven seconds never delivers anything at
    // all, which is a worse failure than the one being fixed.
    let mut inflight = Box::pin(source.list_services(&project));
    // Whether the request now in flight has already gone past the bound at
    // least once. bollard puts its own 120s timeout on every request
    // (`DEFAULT_TIMEOUT`, which `connect_with_defaults` takes), so the request
    // we are deliberately holding does eventually come back -- as an error.
    // Without this, a daemon that hangs for good would be called wedged for
    // 110 seconds, then unreachable for the twelve it takes to notice the next
    // hang, and round again: the bar would send the user off to start a daemon
    // that is plainly running, once every two minutes.
    let mut overran = false;
    loop {
        let waited = tokio::select! {
            _ = cancel.cancelled() => return,
            waited = tokio::time::timeout(POLL_TIMEOUT, &mut inflight) => waited,
        };
        // Whether the request itself finished, which an error counts as and
        // an overrun does not. Read after `waited` is consumed below.
        let request_finished = waited.is_ok();
        let outcome = match waited {
            Ok(Ok(mut services)) => {
                services.retain(|s| cfg.is_visible(&s.name));
                Ok(services)
            }
            Ok(Err(err)) => {
                // The bar can only say *that* the daemon stopped answering.
                // Why is what the supervisor already writes here for its own
                // failures, and this is the only place to look when the note
                // will not clear.
                docker::log_debug(&format!("service poll failed: {err}"));
                // A request that already went quiet on us and has now errored
                // out is the wedged daemon we have been naming all along, not
                // a newly unreachable one.
                Err(if overran {
                    Outage::NotAnswering
                } else {
                    Outage::Unreachable
                })
            }
            Err(_) => {
                docker::log_debug(&format!(
                    "service poll still unanswered after {POLL_TIMEOUT:?}"
                ));
                overran = true;
                Err(Outage::NotAnswering)
            }
        };
        if let Some(poll) = poll_outcome(outcome, &mut health) {
            if tx.send(poll).await.is_err() {
                return;
            }
        }
        if !request_finished {
            // The request is still out there. Go back to waiting on it rather
            // than resting, so the round that finally carries an answer is not
            // delayed by a rest the daemon has already made us take.
            continue;
        }
        // Always rest briefly, so a burst of health-check events can wake
        // the poller early without letting it run continuously.
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(MIN_REFRESH) => {}
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = refresh.notified() => {}
            _ = tokio::time::sleep(REFRESH.saturating_sub(MIN_REFRESH)) => {}
        }
        inflight = Box::pin(source.list_services(&project));
        overran = false;
    }
}

/// Drains input and log events until something asks the app to exit.
///
/// The terminal backend and the input stream are both injected rather than
/// built here, because building either one is what made this loop untestable.
/// `EventStream::new` reads crossterm's global reader, which panics with
/// "reader source not set" in its own constructor, before the first `select!`.
/// Crossterm falls back to `/dev/tty` when stdin is not a terminal, so what is
/// actually missing is a controlling terminal at all -- the normal condition
/// in CI, and the condition the panic in #53 was seen under. A caller that
/// supplies its own stream also controls when input arrives, which is the only
/// way to order events through a `select!` that picks at random among ready
/// branches.
async fn event_loop<B, E>(
    terminal: &mut ratatui::Terminal<B>,
    events: &mut E,
    app: &mut App,
    log_rx: &mut mpsc::Receiver<SourceEvent>,
    svc_rx: &mut mpsc::Receiver<Poll>,
    refresh: &Arc<Notify>,
    cancel: &CancellationToken,
) -> Result<ExitReason>
where
    // ratatui leaves the backend's error type open, so it has to be bounded
    // here for `?` to lift it into `anyhow::Error`. Both backends we use --
    // crossterm's and `TestBackend` -- satisfy this.
    B: ratatui::backend::Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    E: Stream<Item = std::io::Result<Event>> + Unpin,
{
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pinned_applied = false;
    let mut mouse_capture = app.mouse_capture;

    loop {
        // Size the emulators to their panes before drawing, so wrapping matches
        // what the user sees.
        let area = terminal.size().map(|s| ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: s.width,
            height: s.height,
        })?;
        let (_, sizes) = tui::render::layout_for(app, area);
        app.resize_panes(&sizes);

        terminal.draw(|frame| tui::render::draw(app, frame))?;

        if let Some(reason) = app.exit_reason() {
            cancel.cancel();
            return Ok(reason);
        }

        tokio::select! {
            _ = cancel.cancelled() => return Ok(ExitReason::Interrupt),

            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        app.handle_key(key, Instant::now());
                        handle_action(app, area)?;
                        if app.mouse_capture != mouse_capture {
                            mouse_capture = app.mouse_capture;
                            tui::terminal::set_mouse_capture(mouse_capture)?;
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {}
                    Some(Err(err)) => return Err(err.into()),
                    None => return Ok(ExitReason::Quit),
                    _ => {}
                }
            }

            message = log_rx.recv() => {
                let Some(message) = message else { continue };
                apply_source_event(app, message, refresh);
                // Fold whatever else is already queued into the same redraw, but
                // stop at a bound: the rest keeps until the next iteration so
                // rendering and key handling still get a turn.
                let mut drained = 1;
                while drained < MAX_DRAIN_PER_FRAME {
                    match log_rx.try_recv() {
                        Ok(next) => {
                            apply_source_event(app, next, refresh);
                            drained += 1;
                        }
                        Err(_) => break,
                    }
                }
            }

            polled = svc_rx.recv() => {
                if let Some(poll) = polled {
                    // Pins wait for a poll the daemon answered, which is what
                    // the old code waited for too. An answered poll carrying
                    // no services still counts: a project really can have
                    // none, and holding the pins back forever would be worse
                    // than applying them to an empty list.
                    if apply_poll(app, poll) && !pinned_applied {
                        pinned_applied = true;
                        app.apply_startup_pins();
                    }
                }
            }

            _ = ticker.tick() => app.tick(Instant::now()),
        }
    }
}

/// Folds one poll result into the connection health, returning what the UI
/// needs to hear about it, if anything.
fn poll_outcome(
    result: std::result::Result<Vec<model::Service>, Outage>,
    health: &mut ConnectionHealth,
) -> Option<Poll> {
    match result {
        Ok(services) => {
            health.record_success();
            Some(Poll::Services(services))
        }
        Err(kind) => {
            health.record_failure(kind);
            health.outage().map(Poll::Lost)
        }
    }
}

/// Folds one poll outcome into the app, reporting whether services arrived.
fn apply_poll(app: &mut App, poll: Poll) -> bool {
    match poll {
        Poll::Services(services) => {
            // Any answered poll ends an outage, so the note clears itself as
            // soon as the daemon is back rather than waiting for a topology
            // event that a quiet stack may never produce.
            app.set_daemon_outage(None);
            app.set_services(services);
            true
        }
        Poll::Lost(outage) => {
            app.set_daemon_outage(Some(outage));
            false
        }
    }
}

/// Folds one event from the docker layer into the app.
fn apply_source_event(app: &mut App, message: SourceEvent, refresh: &Arc<Notify>) {
    match message {
        SourceEvent::Output {
            service,
            replica,
            // Ignored on purpose. A pane's buffer is keyed on
            // `(service, replica)` and is meant to outlive the container: that
            // shared buffer is what lets a pane keep its history when compose
            // recreates the container behind it. #46 turned down keying it on
            // the ID, proposed as #36, for exactly that reason -- it would
            // hand every recreate a fresh empty buffer and discard the history
            // that surviving a recreate is the point of.
            //
            // Which is not to say this path is free of #50. It holds a partial
            // line too, as a row its emulator's cursor is part way along
            // rather than as a byte buffer, and a recreate splices into it the
            // same way. Ending that row without discarding the buffer is a
            // different mechanism in `LogStore`; #60 tracks it, and the field
            // ignored here is the identity that fix would read.
            container: _,
            bytes,
        } => app.ingest(ServiceKey::new(service, replica), &bytes),
        SourceEvent::Topology => refresh.notify_one(),
    }
}

/// Performs whatever the last key press asked for that the app could not do
/// on its own, such as reaching the clipboard.
fn handle_action(app: &mut App, area: ratatui::layout::Rect) -> Result<()> {
    match app.take_action() {
        Some(Action::CopyOutput) => {
            let message = match app.focused_output() {
                Some(text) if !text.trim().is_empty() => {
                    tui::terminal::copy_to_clipboard(&text)?;
                    "Output copied"
                }
                _ => "Nothing to copy",
            };
            app.set_status_message(message);
        }
        Some(Action::ToggleLayout) => app.toggle_layout_mode(area),
        None => {}
    }
    Ok(())
}

/// Every terminating signal needs the terminal restored before we exit, or the
/// calling script inherits a raw-mode terminal on the alternate screen.
///
/// `SIGINT` is trapped even though `ctrl+c` never produces one here -- raw mode
/// suppresses `ISIG`, so it arrives as a key event. A `SIGINT` sent any other
/// way (`kill -INT`, a process supervisor, a CI harness) would otherwise take
/// the default disposition and skip restoration entirely.
///
/// The number is recorded so the exit status can follow `128 + signo`, letting
/// a supervisor tell its own shutdown from a user quitting.
/// The status to exit with, given whichever signal was recorded.
///
/// `128 + signo` is what a shell reports for a signalled child, so a
/// supervisor can tell a terminating signal from a user pressing `q`.
fn exit_status(signal_exit: &AtomicI32) -> i32 {
    match signal_exit.load(Ordering::SeqCst) {
        signo if signo > 0 => 128 + signo,
        _ => 0,
    }
}

/// Registers the terminating signals, then waits for one in the background.
///
/// Registration happens before this returns, not inside the spawned task.
/// `tokio::spawn` only queues work: the receivers would not exist until the
/// runtime first polled that task, and until they do the default disposition
/// is still in force -- so a signal arriving in the gap would kill the process
/// outright, which is the whole outcome this exists to avoid. The wait itself
/// is what goes in the background.
fn install_signal_handlers(cancel: CancellationToken, signal_exit: Arc<AtomicI32>) -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut int = signal(SignalKind::interrupt()).context("could not handle SIGINT")?;
        let mut term = signal(SignalKind::terminate()).context("could not handle SIGTERM")?;
        let mut hup = signal(SignalKind::hangup()).context("could not handle SIGHUP")?;
        tokio::spawn(async move {
            let signo = tokio::select! {
                _ = int.recv() => SIGINT,
                _ = term.recv() => SIGTERM,
                _ = hup.recv() => SIGHUP,
            };
            signal_exit.store(signo, Ordering::SeqCst);
            cancel.cancel();
        });
    }
    #[cfg(windows)]
    {
        // The same eager registration: `tokio::signal::ctrl_c` is a future
        // that registers on first poll, which is the race this avoids.
        let mut ctrl_c = tokio::signal::windows::ctrl_c().context("could not handle ctrl+c")?;
        tokio::spawn(async move {
            let _ = ctrl_c.recv().await;
            signal_exit.store(SIGINT, Ordering::SeqCst);
            cancel.cancel();
        });
    }
    // Anywhere else there is nothing to register, and the default disposition
    // stands. Every target we ship is unix or windows.
    Ok(())
}

/// Runs a background task, bringing the whole program down if it panics.
///
/// A panic inside a spawned task fires the process-wide panic hook -- which
/// restores the terminal -- while the render loop keeps drawing onto what is now
/// the primary screen in cooked mode, and never exits. Cancelling turns that
/// into a deliberate shutdown.
fn spawn_supervised<F>(cancel: CancellationToken, task: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let handle = tokio::spawn(task);
    tokio::spawn(async move {
        if handle.await.is_err() {
            cancel.cancel();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{Health, Service, ServiceStatus};
    use tui::app::ServiceKey;

    fn app_with_service(name: &str) -> App {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(vec![Service {
            name: name.to_string(),
            replica: 1,
            status: ServiceStatus::Running,
            health: Health::None,
            exit_code: None,
            started_at: None,
            finished_at: None,
        }]);
        app
    }

    #[test]
    fn output_events_are_routed_into_the_matching_buffer() {
        let mut app = app_with_service("api");
        let refresh = Arc::new(Notify::new());
        apply_source_event(
            &mut app,
            SourceEvent::Output {
                service: "api".into(),
                replica: 1,
                container: "api-1".into(),
                bytes: b"hello\r\n".to_vec(),
            },
            &refresh,
        );
        let store = app.store(&ServiceKey::new("api", 1)).expect("a buffer");
        assert!(store.visible_lines().iter().any(|l| l.contains("hello")));
    }

    #[tokio::test]
    async fn topology_events_wake_the_refresher() {
        let mut app = app_with_service("api");
        let refresh = Arc::new(Notify::new());
        apply_source_event(&mut app, SourceEvent::Topology, &refresh);
        // notify_one leaves a permit, so this resolves without waiting.
        tokio::time::timeout(Duration::from_millis(100), refresh.notified())
            .await
            .expect("the refresher should have been notified");
    }

    /// The UI is only told about an outage once the debounce has decided it is
    /// one; before that a failed poll changes nothing on screen.
    #[test]
    fn a_failed_poll_says_nothing_until_the_debounce_agrees() {
        let mut health = ConnectionHealth::default();
        assert!(
            poll_outcome(Err(Outage::Unreachable), &mut health).is_none(),
            "one dropped request must not reach the UI"
        );
    }

    #[test]
    fn a_sustained_outage_is_reported_as_unreachable() {
        let mut health = ConnectionHealth::default();
        let mut sent = None;
        for _ in 0..10 {
            if let Some(poll) = poll_outcome(Err(Outage::Unreachable), &mut health) {
                sent = Some(poll);
                break;
            }
        }
        assert!(
            matches!(sent, Some(Poll::Lost(Outage::Unreachable))),
            "a daemon that never answers must eventually be reported"
        );
    }

    /// A poll that succeeds has to clear the health, or the next single
    /// failure would be treated as the tail of the previous outage.
    #[test]
    fn an_answered_poll_resets_the_failure_run() {
        let mut health = ConnectionHealth::default();
        for _ in 0..10 {
            poll_outcome(Err(Outage::Unreachable), &mut health);
        }
        assert!(matches!(
            poll_outcome(Ok(Vec::new()), &mut health),
            Some(Poll::Services(_))
        ));
        assert!(
            poll_outcome(Err(Outage::Unreachable), &mut health).is_none(),
            "the run must start again after a success"
        );
    }

    #[test]
    fn an_unreachable_poll_marks_the_app_and_brings_no_services() {
        let mut app = app_with_service("api");
        assert!(
            !apply_poll(&mut app, Poll::Lost(Outage::Unreachable)),
            "no services came"
        );
        assert_eq!(app.daemon_outage(), Some(Outage::Unreachable));
        assert_eq!(app.rows().len(), 1, "the last known services must remain");
    }

    /// The boundary the pin comment turns on: an answered poll that happens
    /// to carry nothing is still an answered poll.
    #[test]
    fn an_answered_but_empty_poll_still_counts_as_the_daemon_answering() {
        let mut app = app_with_service("api");
        apply_poll(&mut app, Poll::Lost(Outage::Unreachable));
        assert!(
            apply_poll(&mut app, Poll::Services(Vec::new())),
            "an empty list is an answer, not a silence"
        );
        assert_eq!(app.daemon_outage(), None);
    }

    #[test]
    fn services_arriving_clear_the_unreachable_mark() {
        let mut app = app_with_service("api");
        apply_poll(&mut app, Poll::Lost(Outage::Unreachable));
        assert!(apply_poll(
            &mut app,
            Poll::Services(vec![Service {
                name: "api".to_string(),
                replica: 1,
                status: ServiceStatus::Running,
                health: Health::None,
                exit_code: None,
                started_at: None,
                finished_at: None,
            }])
        ));
        assert_eq!(app.daemon_outage(), None, "recovery must clear the mark");
    }

    // ---- the two loops ---------------------------------------------------
    //
    // #53: everything above tests a fold in isolation, and nothing tested the
    // lines that call them. A mutation that dropped `poll_outcome` from the
    // refresher, or `apply_poll` from the event loop, used to leave the whole
    // suite green while the daemon note never appeared in the real binary.
    // These drive the loops themselves.

    /// A stand-in for the daemon, so the poll loop can be driven without one.
    ///
    /// The cases worth pinning -- a poll that fails, one that is accepted and
    /// never answered, one that is merely slow -- are exactly the ones a real
    /// daemon will not arrange on request.
    struct FakeDaemon {
        /// How this daemon answers.
        behaviour: Behaviour,
        /// Requests started. Tells a request that was left in flight from one
        /// that was cancelled and reissued, which is the whole of #52's
        /// "cancelling a slow poll is a behaviour change in its own right".
        started: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// What a [`FakeDaemon`] does with a request.
    #[derive(Clone)]
    enum Behaviour {
        /// Answers at once, with these services.
        Answers(Vec<Service>),
        /// Fails at once, the way a refused or cut socket does.
        Fails,
        /// Accepts the request and never answers it.
        Hangs,
        /// Answers, but only after this long.
        Slow(Duration, Vec<Service>),
        /// Goes quiet, then fails after this long -- what bollard's own 120s
        /// request timeout does to a request against a wedged daemon.
        QuietThenFails(Duration),
        /// The same, but only for the first request: every one after it fails
        /// at once, the way a daemon that has since been stopped outright
        /// does. Pins that going quiet is remembered per request.
        QuietOnceThenFailsAtOnce(Duration),
    }

    impl FakeDaemon {
        /// A daemon behaving as `behaviour` says, with a fresh request count.
        fn new(behaviour: Behaviour) -> Arc<Self> {
            Arc::new(Self {
                behaviour,
                started: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })
        }
    }

    impl ServiceSource for FakeDaemon {
        fn list_services(
            &self,
            _project: &str,
        ) -> impl std::future::Future<Output = Result<Vec<Service>>> + Send {
            let request = self
                .started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let behaviour = self.behaviour.clone();
            async move {
                match behaviour {
                    Behaviour::Answers(services) => Ok(services),
                    Behaviour::Fails => Err(anyhow::anyhow!("no such file or directory")),
                    Behaviour::Hangs => std::future::pending().await,
                    Behaviour::Slow(delay, services) => {
                        tokio::time::sleep(delay).await;
                        Ok(services)
                    }
                    Behaviour::QuietThenFails(delay) => {
                        tokio::time::sleep(delay).await;
                        Err(anyhow::anyhow!("Timeout error"))
                    }
                    Behaviour::QuietOnceThenFailsAtOnce(delay) => {
                        if request == 0 {
                            tokio::time::sleep(delay).await;
                        }
                        Err(anyhow::anyhow!("no such file or directory"))
                    }
                }
            }
        }
    }

    /// A running service by that name, replica 1.
    fn service(name: &str) -> Service {
        Service {
            name: name.to_string(),
            replica: 1,
            status: ServiceStatus::Running,
            health: Health::None,
            exit_code: None,
            started_at: None,
            finished_at: None,
        }
    }

    /// Starts the refresher against `daemon` and waits for its first message.
    ///
    /// The wait is bounded in virtual time, so a refresher that never speaks
    /// fails the test instead of hanging it. Every caller runs under
    /// `start_paused`, so the hour below costs no real time at all.
    async fn first_poll(daemon: Arc<FakeDaemon>, cfg: Config) -> Poll {
        let (tx, mut rx) = mpsc::channel::<Poll>(4);
        let cancel = CancellationToken::new();
        spawn_refresher(
            daemon,
            "demo".to_string(),
            cfg,
            tx,
            Arc::new(Notify::new()),
            cancel.clone(),
        );
        let received = tokio::time::timeout(Duration::from_secs(3600), rx.recv()).await;
        cancel.cancel();
        received
            .expect("the refresher never told the UI anything")
            .expect("the refresher stopped without sending")
    }

    /// The mutation #53 names: make the refresher skip `poll_outcome` and only
    /// ever send `Poll::Services`, and this is what notices.
    #[tokio::test(start_paused = true)]
    async fn a_refresher_whose_polls_fail_reports_an_outage() {
        let poll = first_poll(FakeDaemon::new(Behaviour::Fails), Config::default()).await;
        assert!(
            matches!(poll, Poll::Lost(Outage::Unreachable)),
            "a failing poll must reach the UI as an outage, got {poll:?}"
        );
    }

    /// The other half of the same wiring: an answered poll has to arrive as
    /// services, filtered the way the config asks.
    #[tokio::test(start_paused = true)]
    async fn a_refresher_that_is_answered_sends_the_configured_services() {
        let cfg = Config {
            exclude: vec!["db".to_string()],
            ..Config::default()
        };
        let daemon = FakeDaemon::new(Behaviour::Answers(vec![service("api"), service("db")]));
        let poll = first_poll(daemon, cfg).await;
        let Poll::Services(services) = poll else {
            panic!("an answered poll must arrive as services, got {poll:?}");
        };
        let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["api"], "the excluded service must not be sent");
    }

    /// #52: a daemon that accepts the connection and then never answers used
    /// to produce nothing at all, because no poll ever completed and
    /// `poll_outcome` was never reached.
    #[tokio::test(start_paused = true)]
    async fn a_daemon_that_never_answers_is_reported_as_not_answering() {
        let daemon = FakeDaemon::new(Behaviour::Hangs);
        let started = daemon.started.clone();
        let poll = first_poll(daemon, Config::default()).await;
        assert!(
            matches!(poll, Poll::Lost(Outage::NotAnswering)),
            "a hung daemon must be reported, and not as unreachable, got {poll:?}"
        );
        assert_eq!(
            started.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "giving up on waiting must not reissue the request"
        );
    }

    /// bollard puts a 120s timeout on every request, so the request this loop
    /// holds in flight against a wedged daemon does come back -- as an error.
    /// Calling that "unreachable" would march the note back and forth between
    /// two contradictory claims about one unchanging daemon, and send the user
    /// to start one that is plainly running.
    #[tokio::test(start_paused = true)]
    async fn a_wedged_daemon_is_never_reported_as_unreachable() {
        // Long enough for the wait to overrun several times over first, which
        // is what bollard's own timeout does at twelve times POLL_TIMEOUT.
        let daemon = FakeDaemon::new(Behaviour::QuietThenFails(POLL_TIMEOUT * 12));
        let (tx, mut rx) = mpsc::channel::<Poll>(64);
        let cancel = CancellationToken::new();
        spawn_refresher(
            daemon,
            "demo".to_string(),
            Config::default(),
            tx,
            Arc::new(Notify::new()),
            cancel.clone(),
        );
        // Two of bollard's timeouts' worth, so the round after the error --
        // where the misclassification showed up -- is included.
        let mut seen = Vec::new();
        let watch = async {
            while seen.len() < 30 {
                match rx.recv().await {
                    Some(poll) => seen.push(poll),
                    None => break,
                }
            }
        };
        let _ = tokio::time::timeout(POLL_TIMEOUT * 30, watch).await;
        cancel.cancel();
        assert!(
            !seen.is_empty(),
            "the refresher must report a wedged daemon at all"
        );
        assert!(
            seen.iter()
                .all(|p| matches!(p, Poll::Lost(Outage::NotAnswering))),
            "a daemon that only ever went quiet was called something else: {seen:?}"
        );
    }

    /// The pacing between rounds, both halves of it at once. The floor is what
    /// stops a burst of container events turning the poller into a hot loop
    /// against the daemon; the wake is what stops a topology change leaving
    /// the sidebar stale for the full refresh period. Both are one line in the
    /// loop, and nothing else exercises either.
    #[tokio::test(start_paused = true)]
    async fn a_topology_event_wakes_the_poller_early_but_no_sooner_than_the_floor() {
        let daemon = FakeDaemon::new(Behaviour::Answers(Vec::new()));
        let refresh = Arc::new(Notify::new());
        let (tx, mut rx) = mpsc::channel::<Poll>(8);
        let cancel = CancellationToken::new();
        spawn_refresher(
            daemon,
            "demo".to_string(),
            Config::default(),
            tx,
            refresh.clone(),
            cancel.clone(),
        );

        let started = tokio::time::Instant::now();
        rx.recv().await.expect("the first poll");
        // `notify_one` leaves a permit whether or not the poller is waiting
        // yet, so this does not race the loop reaching its rest.
        refresh.notify_one();
        rx.recv().await.expect("the second poll");
        let between = started.elapsed();
        cancel.cancel();

        assert!(
            between >= MIN_REFRESH,
            "the floor must hold, or a burst of events becomes a hot loop: {between:?}"
        );
        assert!(
            between < REFRESH,
            "a topology event must not wait out the whole refresh period: {between:?}"
        );
    }

    /// `POLL_TIMEOUT`'s doc argues for ten seconds from measurements, the same
    /// way `FAILURES_BEFORE_OUTAGE`'s argues for three. The other tests scale
    /// off the constant and so hold for any value; this one is longhand,
    /// because the value is a promise about how long the screen may go on
    /// showing statuses nothing is refreshing.
    #[test]
    fn the_poll_waits_ten_seconds_exactly() {
        assert_eq!(POLL_TIMEOUT, Duration::from_secs(10));
    }

    /// The mirror of the case above, and the reason going quiet is remembered
    /// per request rather than for good: a daemon that wedged once and has
    /// since been stopped outright has to be called unreachable again. Keeping
    /// the old note would send the user away from the remedy that works.
    #[tokio::test(start_paused = true)]
    async fn a_wedged_daemon_that_is_then_stopped_is_called_unreachable_again() {
        // Three bounds and a bit: long enough that the first request is
        // reported before it errors, so the flag is set when it does.
        let quiet = POLL_TIMEOUT * 3 + Duration::from_secs(1);
        let daemon = FakeDaemon::new(Behaviour::QuietOnceThenFailsAtOnce(quiet));
        let (tx, mut rx) = mpsc::channel::<Poll>(64);
        let cancel = CancellationToken::new();
        spawn_refresher(
            daemon,
            "demo".to_string(),
            Config::default(),
            tx,
            Arc::new(Notify::new()),
            cancel.clone(),
        );
        let mut seen = Vec::new();
        let watch = async {
            loop {
                match rx.recv().await {
                    Some(poll) => {
                        let done = matches!(poll, Poll::Lost(Outage::Unreachable));
                        seen.push(poll);
                        if done {
                            return;
                        }
                    }
                    None => return,
                }
            }
        };
        let reached = tokio::time::timeout(POLL_TIMEOUT * 30, watch).await;
        cancel.cancel();
        assert!(
            matches!(seen.first(), Some(Poll::Lost(Outage::NotAnswering))),
            "the wedge had to be reported as a wedge first: {seen:?}"
        );
        assert!(
            reached.is_ok(),
            "a daemon that is now simply gone was never called unreachable: \
             {} polls, none of them it, the first being {:?}",
            seen.len(),
            seen.first()
        );
    }

    /// The cost #52 warns about, refused: a poll that overruns the bound is
    /// left in flight, so a daemon that is merely slow still delivers its
    /// services and says nothing to the user.
    #[tokio::test(start_paused = true)]
    async fn a_poll_slower_than_the_bound_still_delivers_its_services() {
        // Half again as long as the bound: one overrun, which on its own is
        // two short of the debounce, so nothing is ever reported.
        let slow = POLL_TIMEOUT + POLL_TIMEOUT / 2;
        let daemon = FakeDaemon::new(Behaviour::Slow(slow, vec![service("api")]));
        let started = daemon.started.clone();
        let poll = first_poll(daemon, Config::default()).await;
        let Poll::Services(services) = poll else {
            panic!("a slow poll must still deliver, got {poll:?}");
        };
        assert_eq!(services.len(), 1);
        assert_eq!(
            started.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the overrun must not have thrown the request away and started another"
        );
    }

    /// What the frame said when the loop stopped drawing, one row per line.
    fn frame_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The frame the event-loop tests draw into. Wide enough that an output
    /// pane is comfortably wider than a `LogStore`'s own starting width, which
    /// is what lets a test tell a pane that was sized from one that was not.
    const TEST_FRAME: (u16, u16) = (200, 40);

    /// One key press, as the input stream carries it.
    fn key(code: crossterm::event::KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    /// Runs `event_loop` against a test backend and `events`, with `polls` and
    /// `logs` already queued, and hands back whatever it returned -- `None` if
    /// it was still running at the deadline -- and the last frame.
    ///
    /// Deterministic because time is paused. The ticker is the only arm that
    /// becomes ready on the clock, and a paused clock only advances once every
    /// task is idle -- which cannot happen while a queued message still leaves
    /// the loop with work to do. So everything queued here is drained before
    /// the loop can go quiet, whatever order `select!` picks along the way.
    ///
    /// The channel senders are held for the whole call on purpose: a closed
    /// channel resolves immediately and for ever, and the arms that read them
    /// would spin.
    async fn drive_event_loop_over<E>(
        app: &mut App,
        polls: Vec<Poll>,
        logs: Vec<SourceEvent>,
        events: &mut E,
        cancel: &CancellationToken,
    ) -> (Option<Result<ExitReason>>, String)
    where
        E: Stream<Item = std::io::Result<Event>> + Unpin,
    {
        let backend = ratatui::backend::TestBackend::new(TEST_FRAME.0, TEST_FRAME.1);
        let mut terminal = ratatui::Terminal::new(backend).expect("a test terminal");

        let (svc_tx, mut svc_rx) = mpsc::channel::<Poll>(16);
        for poll in polls {
            svc_tx.try_send(poll).expect("room for the queued polls");
        }
        let (log_tx, mut log_rx) = mpsc::channel::<SourceEvent>(16);
        for log in logs {
            log_tx.try_send(log).expect("room for the queued events");
        }

        let refresh = Arc::new(Notify::new());
        // A deadline rather than a quit key, so a test that wants to inspect
        // the app after the loop has digested its input does not have to race
        // the exit against the input. Virtual seconds, but not free: a test
        // that never exits redraws on the loop's 100ms ticker until the
        // deadline, fifty frames of it, which measures around a fifth of a
        // second of real time apiece.
        let exit = tokio::time::timeout(
            Duration::from_secs(5),
            event_loop(
                &mut terminal,
                events,
                app,
                &mut log_rx,
                &mut svc_rx,
                &refresh,
                cancel,
            ),
        )
        .await
        .ok();
        drop((svc_tx, log_tx));
        (exit, frame_text(&terminal))
    }

    /// `drive_event_loop_over` with an input channel built for `input`, which
    /// is what all but the exit-path tests want.
    ///
    /// The sender is held until the loop is done, because a stream that has
    /// ended resolves immediately and for ever -- the `None` race #53
    /// describes, which only one test wants on purpose.
    async fn drive_event_loop(
        app: &mut App,
        polls: Vec<Poll>,
        logs: Vec<SourceEvent>,
        input: Vec<Event>,
    ) -> (Option<ExitReason>, String) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<std::io::Result<Event>>();
        for event in input {
            tx.unbounded_send(Ok(event)).expect("the receiver is alive");
        }
        let cancel = CancellationToken::new();
        let (exit, frame) = drive_event_loop_over(app, polls, logs, &mut rx, &cancel).await;
        drop(tx);
        (
            exit.transpose().expect("the event loop must not fail"),
            frame,
        )
    }

    /// The mutation #53 names for the event loop: inline the old `set_services`
    /// into the `svc_rx` arm and drop both `set_daemon_outage` calls -- which
    /// #53 names as `set_daemon_reachable`, the method they were before this.
    /// The note then never appears in the real binary, and this is what
    /// notices.
    #[tokio::test(start_paused = true)]
    async fn the_event_loop_puts_an_outage_on_the_frame() {
        let mut app = app_with_service("api");
        let (_, frame) = drive_event_loop(
            &mut app,
            vec![Poll::Lost(Outage::NotAnswering)],
            Vec::new(),
            Vec::new(),
        )
        .await;
        assert_eq!(app.daemon_outage(), Some(Outage::NotAnswering));
        assert!(
            frame.contains("Docker daemon not answering"),
            "the outage never reached the screen:\n{frame}"
        );
    }

    /// The recovery direction, and the `set_services` call in the same arm.
    #[tokio::test(start_paused = true)]
    async fn the_event_loop_applies_an_answered_poll_and_clears_the_note() {
        let mut app = app_with_service("api");
        // Marked here rather than through a first poll, so that clearing it is
        // what the assertion turns on. A loop that ignored the outage poll as
        // well would otherwise pass this by never setting the note at all.
        app.set_daemon_outage(Some(Outage::Unreachable));
        let (_, frame) = drive_event_loop(
            &mut app,
            vec![Poll::Services(vec![service("api"), service("worker")])],
            Vec::new(),
            Vec::new(),
        )
        .await;
        assert_eq!(app.daemon_outage(), None, "an answer must clear the note");
        assert_eq!(app.rows().len(), 2, "the poll's services must be applied");
        assert!(
            !frame.contains("Docker daemon"),
            "the note outlived the outage:\n{frame}"
        );
    }

    /// The pins wait for an answered poll, and nothing tested that they are
    /// ever applied at all -- the call sits in the same arm.
    #[tokio::test(start_paused = true)]
    async fn the_event_loop_applies_the_startup_pins_once_a_poll_answers() {
        let cfg = Config {
            pinned: vec!["api".to_string()],
            ..Config::default()
        };
        let mut app = App::new("demo", &cfg);
        assert_eq!(app.pane_key(0), None, "nothing is pinned before a poll");
        drive_event_loop(
            &mut app,
            vec![Poll::Services(vec![service("api")])],
            Vec::new(),
            Vec::new(),
        )
        .await;
        assert_eq!(
            app.pane_key(0).map(|k| k.name),
            Some("api".to_string()),
            "an answered poll must release the startup pins"
        );
    }

    /// The `log_rx` arm, which dispatches `apply_source_event`.
    #[tokio::test(start_paused = true)]
    async fn the_event_loop_routes_log_output_into_its_buffer() {
        let mut app = app_with_service("api");
        drive_event_loop(
            &mut app,
            Vec::new(),
            vec![SourceEvent::Output {
                service: "api".into(),
                replica: 1,
                bytes: b"hello from the loop\r\n".to_vec(),
            }],
            Vec::new(),
        )
        .await;
        let store = app.store(&ServiceKey::new("api", 1)).expect("a buffer");
        assert!(
            store
                .visible_lines()
                .iter()
                .any(|l| l.contains("hello from the loop")),
            "the log event never reached the buffer"
        );
    }

    /// `handle_action` is the other half of the key arm: `handle_key` only
    /// records what the app could not do on its own, and this is what carries
    /// it out. Dropping the call leaves copy-output and the layout toggle
    /// silently doing nothing, which is #53's complaint exactly.
    #[tokio::test(start_paused = true)]
    async fn the_event_loop_carries_out_the_action_a_key_asked_for() {
        let mut app = app_with_service("api");
        app.open_and_focus_selection();
        let area = ratatui::layout::Rect::new(0, 0, TEST_FRAME.0, TEST_FRAME.1);
        let before = app.layout().calculate(area, 1);
        drive_event_loop(
            &mut app,
            Vec::new(),
            Vec::new(),
            vec![key(crossterm::event::KeyCode::Char('m'))],
        )
        .await;
        let after = app.layout().calculate(area, 1);
        assert_ne!(
            before.panes[0], after.panes[0],
            "the layout toggle never left the queue"
        );
    }

    /// Sizing the emulators to their panes happens at the top of the loop, and
    /// nothing but the loop does it. Without it a pane wraps its output at a
    /// `LogStore`'s starting width rather than the width on screen, so what
    /// the user reads is broken in a place they would blame the service for.
    #[tokio::test(start_paused = true)]
    async fn the_event_loop_sizes_a_pane_before_anything_is_written_to_it() {
        let mut app = app_with_service("api");
        app.open_and_focus_selection();
        let area = ratatui::layout::Rect::new(0, 0, TEST_FRAME.0, TEST_FRAME.1);
        let (_, sizes) = tui::render::layout_for(&app, area);
        let cols = sizes.first().expect("an open pane").2;
        // The premise: a line this long fits the pane and does not fit a store
        // that was never told about the pane. Asserted rather than assumed, so
        // a layout change makes this say so instead of passing vacuously.
        let line = "x".repeat(100);
        assert!(
            cols > line.len() as u16,
            "the frame is too narrow for this test to mean anything: {cols} columns"
        );

        drive_event_loop(
            &mut app,
            Vec::new(),
            vec![SourceEvent::Output {
                service: "api".into(),
                replica: 1,
                bytes: format!("{line}\r\n").into_bytes(),
            }],
            Vec::new(),
        )
        .await;

        let store = app.store(&ServiceKey::new("api", 1)).expect("a buffer");
        assert!(
            store.visible_lines().iter().any(|l| l.contains(&line)),
            "the line was wrapped at some width other than the pane's"
        );
    }

    /// The `None` arm, which #53 names as the one a test would have raced. An
    /// ended stream is deliberate here, and the only place it is.
    #[tokio::test(start_paused = true)]
    async fn an_input_stream_that_ends_quits_the_loop() {
        let mut app = app_with_service("api");
        let mut ended = futures::stream::iter(Vec::<std::io::Result<Event>>::new());
        let cancel = CancellationToken::new();
        let (exit, _) =
            drive_event_loop_over(&mut app, Vec::new(), Vec::new(), &mut ended, &cancel).await;
        assert!(
            matches!(exit, Some(Ok(ExitReason::Quit))),
            "input running out is a quit, not an interrupt: {exit:?}"
        );
    }

    /// The other exit, which carries a different status to the calling script.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_token_stops_the_loop_as_an_interrupt() {
        let mut app = app_with_service("api");
        let mut waiting = futures::stream::pending::<std::io::Result<Event>>();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (exit, _) =
            drive_event_loop_over(&mut app, Vec::new(), Vec::new(), &mut waiting, &cancel).await;
        assert!(
            matches!(exit, Some(Ok(ExitReason::Interrupt))),
            "a cancelled run is an interrupt, not a quit: {exit:?}"
        );
    }

    /// A broken input stream is reported rather than treated as an exit, so
    /// the terminal is restored and the failure reaches the caller.
    #[tokio::test(start_paused = true)]
    async fn an_input_stream_that_fails_is_reported() {
        let mut app = app_with_service("api");
        let mut broken = futures::stream::iter(vec![Err(std::io::Error::other("the tty went"))]);
        let cancel = CancellationToken::new();
        let (exit, _) =
            drive_event_loop_over(&mut app, Vec::new(), Vec::new(), &mut broken, &cancel).await;
        let Some(Err(err)) = exit else {
            panic!("a failed read must not look like an ordinary exit: {exit:?}");
        };
        assert!(err.to_string().contains("the tty went"), "got {err}");
    }

    /// Only presses are acted on, and `KeyEventKind` has three variants, not
    /// two. Windows reports a repeat for a held key and a release for every
    /// key, so anything looser than "is a press" hands the app the same
    /// keystroke more than once -- and `q` would quit from a key the user is
    /// still holding, or has just let go of.
    ///
    /// Both kinds, because either one alone leaves a loosening that passes:
    /// excluding only releases still admits repeats, and vice versa.
    #[tokio::test(start_paused = true)]
    async fn a_key_that_is_not_being_pressed_is_not_a_key_press() {
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            let mut app = app_with_service("api");
            let event = Event::Key(crossterm::event::KeyEvent::new_with_kind(
                crossterm::event::KeyCode::Char('q'),
                crossterm::event::KeyModifiers::NONE,
                kind,
            ));
            let (exit, _) = drive_event_loop(&mut app, Vec::new(), Vec::new(), vec![event]).await;
            assert_eq!(exit, None, "{kind:?} must not be handled as a press");
        }
    }

    /// The last arm with nothing holding it. The ticker is what advances the
    /// throbber and the uptimes, ages a status message out, and paces the
    /// auto-exit countdown -- so a loop that stopped calling `tick` would look
    /// frozen in precisely the way #19 was opened about.
    #[tokio::test(start_paused = true)]
    async fn the_event_loop_goes_on_ticking_the_app() {
        let mut app = app_with_service("api");
        let before = app.throbber();
        drive_event_loop(&mut app, Vec::new(), Vec::new(), Vec::new()).await;
        // More than one, so a single tick could not pass for a running clock.
        assert!(
            app.throbber() > before + 1,
            "the throbber moved {} times across the whole deadline",
            app.throbber() - before
        );
    }

    /// The key arm, and the exit it produces. `q` is dispatched through the
    /// injected stream, which is the whole point of injecting one.
    #[tokio::test(start_paused = true)]
    async fn the_event_loop_quits_on_a_key_from_its_input_stream() {
        let mut app = app_with_service("api");
        let (exit, _) = drive_event_loop(
            &mut app,
            Vec::new(),
            Vec::new(),
            vec![key(crossterm::event::KeyCode::Char('q'))],
        )
        .await;
        assert_eq!(exit, Some(ExitReason::Quit));
    }

    fn press(app: &mut App, code: crossterm::event::KeyCode) {
        app.handle_key(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            Instant::now(),
        );
    }

    #[test]
    fn copying_a_pane_with_no_output_reports_nothing_to_copy() {
        let mut app = app_with_service("api");
        // Focus a pane so `c` queues a copy, but ingest nothing, so the buffer
        // is empty and the clipboard is never touched.
        app.open_and_focus_selection();
        press(&mut app, crossterm::event::KeyCode::Char('c'));
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        handle_action(&mut app, area).unwrap();
        assert_eq!(app.status_message(), Some("Nothing to copy"));
    }

    #[test]
    fn toggling_the_layout_changes_the_frame_geometry() {
        let mut app = app_with_service("api");
        let area = ratatui::layout::Rect::new(0, 0, 160, 40);
        app.open_and_focus_selection();
        let before = app.layout().calculate(area, 1);

        press(&mut app, crossterm::event::KeyCode::Char('m'));
        handle_action(&mut app, area).unwrap();
        let after = app.layout().calculate(area, 1);

        assert_ne!(
            before.panes[0], after.panes[0],
            "toggling should move the pane between stacked and side-by-side"
        );
    }
}
