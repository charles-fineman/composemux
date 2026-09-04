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
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;
use tui::app::{Action, App, ExitReason, ServiceKey};

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

    let cancel = CancellationToken::new();
    install_signal_handlers(cancel.clone());

    // A full-screen UI is useless when output is piped, and would write escape
    // sequences into whatever is capturing it.
    if args.no_tui || !std::io::stdout().is_terminal() {
        fallback::run(&client, &project, cfg.tail, cancel).await?;
        return Ok(0);
    }

    run_tui(client, project, cfg, cancel).await
}

async fn run_tui(
    client: DockerClient,
    project: String,
    cfg: Config,
    cancel: CancellationToken,
) -> Result<i32> {
    let client = Arc::new(client);
    let mut app = App::new(&project, &cfg);

    let (log_tx, mut log_rx) = mpsc::channel::<SourceEvent>(4096);
    let supervisor = LogSupervisor::new(&client, &project, cfg.tail, log_tx);
    let supervisor_cancel = cancel.clone();
    tokio::spawn(async move { supervisor.run(supervisor_cancel).await });

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
    Ok(exit.code())
}

fn spawn_refresher(
    client: Arc<DockerClient>,
    project: String,
    cfg: Config,
    tx: mpsc::Sender<Vec<model::Service>>,
    refresh: Arc<Notify>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
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

/// Ctrl-C and SIGTERM both need the terminal restored before we exit, or the
/// calling CLI inherits a raw-mode terminal.
fn install_signal_handlers(cancel: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => return,
            };
            let mut hup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(_) => return,
            };
            tokio::select! {
                _ = term.recv() => cancel.cancel(),
                _ = hup.recv() => cancel.cancel(),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
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
