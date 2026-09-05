mod config;
mod docker;
mod fallback;
mod model;
mod project;
mod tui;

use anyhow::{bail, Context, Result};
use clap::Parser;
use config::Config;
use crossterm::event::{Event, EventStream, KeyEventKind};
use docker::{DockerClient, LogSupervisor, SourceEvent};
use futures::StreamExt;
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
#[cfg(unix)]
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
#[cfg(unix)]
const SIGTERM: i32 = 15;

/// Animation tick. Also paces the auto-exit countdown.
const TICK: Duration = Duration::from_millis(100);
/// How often the service list is re-read for status and uptime.
const REFRESH: Duration = Duration::from_secs(2);
/// Floor on how often a refresh may run, so a burst of container events cannot
/// turn the poller into a hot loop against the daemon.
const MIN_REFRESH: Duration = Duration::from_millis(500);
/// Most log messages folded into one redraw. Without a cap, a service logging
/// faster than we can render would keep the drain loop from ever returning, and
/// the UI would stop responding to keys.
const MAX_DRAIN_PER_FRAME: usize = 512;

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
    let (svc_tx, mut svc_rx) = mpsc::channel::<Vec<model::Service>>(4);
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
    let result = event_loop(
        &mut terminal,
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

fn spawn_refresher(
    client: Arc<DockerClient>,
    project: String,
    cfg: Config,
    tx: mpsc::Sender<Vec<model::Service>>,
    refresh: Arc<Notify>,
    cancel: CancellationToken,
) {
    let watchdog = cancel.clone();
    spawn_supervised(watchdog, async move {
        loop {
            if let Ok(mut services) = client.list_services(&project).await {
                services.retain(|s| cfg.is_visible(&s.name));
                if tx.send(services).await.is_err() {
                    return;
                }
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
        }
    });
}

async fn event_loop(
    terminal: &mut tui::terminal::Tui,
    app: &mut App,
    log_rx: &mut mpsc::Receiver<SourceEvent>,
    svc_rx: &mut mpsc::Receiver<Vec<model::Service>>,
    refresh: &Arc<Notify>,
    cancel: &CancellationToken,
) -> Result<ExitReason> {
    let mut events = EventStream::new();
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

            services = svc_rx.recv() => {
                if let Some(services) = services {
                    app.set_services(services);
                    if !pinned_applied {
                        pinned_applied = true;
                        app.apply_startup_pins();
                    }
                }
            }

            _ = ticker.tick() => app.tick(Instant::now()),
        }
    }
}

fn apply_source_event(app: &mut App, message: SourceEvent, refresh: &Arc<Notify>) {
    match message {
        SourceEvent::Output {
            service,
            replica,
            bytes,
        } => app.ingest(ServiceKey::new(service, replica), &bytes),
        SourceEvent::Topology => refresh.notify_one(),
    }
}

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
