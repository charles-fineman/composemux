#![allow(clippy::missing_docs_in_private_items)] // 7 left to document
//! The bottom bar: progress on the left, context in the middle, keys on the right.
//!
//! Ported from nx `packages/nx/src/native/tui/components/status_bar.rs` and
//! `help_text.rs` (MIT, (c) 2017-2026 Narwhal Technologies Inc.)

use crate::docker::Outage;
use crate::model::ServiceStatus;
use crate::tui::app::App;
use crate::tui::filter::FilterState;
use crate::tui::focus::Focus;
use crate::tui::theme::THEME;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

const MIN_HELP_WIDTH: u16 = 16;
const BOTTOM_SPACING: u16 = 4;
const RIGHT_MARGIN: u16 = 1;
const STATUS_MIN_WIDTH: u16 = 25;

pub const HEIGHT: u16 = 1;

/// Right-hand key hints, in drop order: the leftmost goes first as space runs out.
fn help_items(app: &App) -> Vec<(&'static str, &'static str)> {
    match app.focus() {
        // The two labels are nx's own: "full screen: <enter>" on a focused
        // pane, "exit: esc" once that pane has the frame. nx shows the exit
        // hint on its own, but only because its full screen is a separate
        // stripped-down app; here the rest of the row is still true.
        Focus::Pane(_) if app.full_screen_pane().is_some() => vec![
            ("scroll: ", "↑ ↓"),
            ("copy: ", "c"),
            ("exit: ", "esc"),
            ("quit: ", "q"),
            ("help: ", "?"),
        ],
        Focus::Pane(_) => vec![
            ("scroll: ", "↑ ↓"),
            ("copy: ", "c"),
            ("full screen: ", "<enter>"),
            ("quit: ", "q"),
            ("help: ", "?"),
        ],
        _ => vec![
            ("pin output: ", "1 or 2"),
            ("show output: ", "<enter>"),
            ("filter: ", "/"),
            ("navigate: ", "↑ ↓"),
            ("quit: ", "q"),
            ("help: ", "?"),
        ],
    }
}

/// Builds the hint line, dropping leading items until it fits.
fn help_line(app: &App, available: u16) -> Line<'static> {
    let items = help_items(app);
    for start in 0..items.len() {
        let spans = render_items(&items[start..]);
        let width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        if width as u16 <= available {
            return Line::from(spans);
        }
    }
    Line::from("")
}

fn render_items(items: &[(&'static str, &'static str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (label, key)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            *label,
            Style::default().fg(THEME.secondary_fg),
        ));
        spans.push(Span::styled(*key, Style::default().fg(THEME.info)));
    }
    spans
}

/// Left slot: how many services are up, and the project name.
fn status_line(app: &App) -> Line<'static> {
    let total = app.rows().len();
    if total == 0 {
        return Line::from("");
    }
    let running = app
        .rows()
        .iter()
        .filter(|r| {
            matches!(
                r.service.status,
                ServiceStatus::Running | ServiceStatus::Unhealthy
            )
        })
        .count();
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("{running}/{total}"),
            Style::default().fg(THEME.secondary_fg),
        ),
        Span::styled(" up", Style::default().add_modifier(Modifier::DIM)),
    ])
}

/// Middle slot, in precedence order: transient message, countdown, then filter.
fn context_line(app: &App) -> Line<'static> {
    if let Some(message) = app.status_message() {
        return Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(THEME.info),
        ));
    }
    if let Some(secs) = app.countdown_remaining() {
        return Line::from(Span::styled(
            format!("All services exited - closing in {secs}s (any key cancels)"),
            Style::default().fg(THEME.warning),
        ));
    }
    // Above the filter, which only restates something the user typed and can
    // retype, and below the countdown, which is the sole warning that the app
    // is about to close itself. Warning rather than error: the supervisor and
    // the poller both keep retrying, so this is a condition being handled.
    if let Some(outage) = app.daemon_outage() {
        // Three notes, because they send the user to different places: an
        // unreachable daemon is one to start, one that is not answering has
        // taken the request and gone quiet and is very likely running, and one
        // that rejected the request is running and talking to us. Calling
        // either of the last two "unreachable" would send the user to check
        // something that is fine. Which error the daemon gave does not fit
        // here -- the debug log has it, and the README says so.
        return Line::from(Span::styled(
            match outage {
                Outage::Unreachable => "Docker daemon unreachable - retrying",
                Outage::NotAnswering => "Docker daemon not answering - retrying",
                Outage::Rejected => "Docker daemon rejected the request - retrying",
            },
            Style::default().fg(THEME.warning),
        ));
    }
    match app.filter().state() {
        FilterState::Editing => Line::from(vec![
            Span::styled(
                format!("/{}", app.filter().query()),
                Style::default().fg(THEME.info),
            ),
            Span::styled(
                format!(
                    "  {} filtered out   <enter> confirm, <esc> cancel",
                    app.hidden_count()
                ),
                Style::default().fg(THEME.secondary_fg),
            ),
        ]),
        FilterState::Persisted => Line::from(vec![
            Span::styled(
                format!("/{}", app.filter().query()),
                Style::default().fg(THEME.info),
            ),
            Span::styled(
                format!("  {} hidden (/ to edit)", app.hidden_count()),
                Style::default().fg(THEME.secondary_fg),
            ),
        ]),
        FilterState::Off => Line::from(""),
    }
}

pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let status = status_line(app);
    let status_width = (status.width() as u16).clamp(1, STATUS_MIN_WIDTH);
    let context = context_line(app);
    let available = area
        .width
        .saturating_sub(status_width + BOTTOM_SPACING + RIGHT_MARGIN);
    // The middle slot takes its natural width before the hints do, and only
    // the essential hints are held back for it, exactly as nx orders the two
    // in `render_single_line`. Letting the hints go first starved the slot:
    // 24 columns at 120, and 4 at both 80 and 100, which truncated every
    // message it has ever carried.
    //
    // Floor the budget at the minimum hint width, but never above the space
    // that actually exists, or the hints would overflow instead of degrading.
    let help_budget = available
        .saturating_sub(context.width() as u16)
        .max(MIN_HELP_WIDTH)
        .min(available.max(1));
    let help = help_line(app, help_budget);
    let help_width = help.width() as u16;

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(status_width),
            Constraint::Length(BOTTOM_SPACING),
            Constraint::Fill(1),
            Constraint::Length(help_width + RIGHT_MARGIN),
        ])
        .split(area);

    Widget::render(Paragraph::new(status), chunks[0], buf);
    Widget::render(Paragraph::new(context), chunks[2], buf);
    Widget::render(Paragraph::new(help).right_aligned(), chunks[3], buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Health, Service};

    fn app_with(names: &[&str]) -> App {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(
            names
                .iter()
                .map(|n| Service {
                    name: n.to_string(),
                    replica: 1,
                    status: ServiceStatus::Running,
                    health: Health::None,
                    exit_code: None,
                    started_at: None,
                    finished_at: None,
                })
                .collect(),
        );
        app
    }

    #[test]
    fn the_hint_line_drops_items_from_the_left_when_cramped() {
        let app = app_with(&["a"]);
        let wide = help_line(&app, 200);
        let narrow = help_line(&app, 20);
        assert!(narrow.width() < wide.width());
        let text: String = narrow.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("help: ?"), "the last items survive: {text}");
    }

    #[test]
    fn an_impossibly_narrow_bar_renders_nothing_rather_than_overflowing() {
        let app = app_with(&["a"]);
        assert_eq!(help_line(&app, 1).width(), 0);
    }

    #[test]
    fn pane_focus_shows_pane_specific_hints() {
        let mut app = app_with(&["a"]);
        app.open_and_focus_selection();
        let text: String = help_line(&app, 200)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("scroll"));
        assert!(text.contains("copy"));
        assert!(!text.contains("pin output"));
    }

    #[test]
    /// The status bar is where the binding is found in passing, so it has to
    /// carry nx's wording rather than our own.
    fn a_focused_pane_advertises_full_screen() {
        let mut app = app_with(&["a"]);
        app.open_and_focus_selection();
        let text: String = help_line(&app, 200)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("full screen: <enter>"), "got: {text}");
    }

    #[test]
    /// The bar follows the mode: once inside, the useful hint is how to leave.
    fn a_full_screen_pane_advertises_the_way_out() {
        let mut app = app_with(&["a"]);
        app.open_and_focus_selection();
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
            std::time::Instant::now(),
        );
        let text: String = help_line(&app, 200)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("exit: esc"), "got: {text}");
        assert!(
            !text.contains("full screen: <enter>"),
            "the pane is already full screen: {text}"
        );
    }

    #[test]
    fn the_status_slot_counts_running_services() {
        let app = app_with(&["a", "b"]);
        let text: String = status_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("2/2 up"), "got {text}");
    }

    #[test]
    fn an_empty_stack_shows_no_status() {
        let cfg = Config::default();
        let app = App::new("demo", &cfg);
        assert_eq!(status_line(&app).width(), 0);
    }

    #[test]
    fn the_filter_query_appears_in_the_context_slot() {
        let mut app = app_with(&["api", "worker"]);
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('/'),
                crossterm::event::KeyModifiers::NONE,
            ),
            std::time::Instant::now(),
        );
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            ),
            std::time::Instant::now(),
        );
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.starts_with("/a"), "got {text}");
        assert!(text.contains("filtered out"));
    }

    /// The whole of #19: a frozen screen and a spinning throbber look exactly
    /// like a quiet stack, so the bar has to say which it is.
    #[test]
    fn an_unreachable_daemon_is_named_in_the_context_slot() {
        let mut app = app_with(&["a"]);
        app.set_daemon_outage(Some(Outage::Unreachable));
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains("Docker daemon unreachable"),
            "the bar must name the daemon, got {text:?}"
        );
    }

    /// It has to say it is still trying, or the only reasonable reading is
    /// that the tool has given up and should be restarted.
    #[test]
    fn the_note_says_the_connection_is_being_retried() {
        let mut app = app_with(&["a"]);
        app.set_daemon_outage(Some(Outage::Unreachable));
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("retrying"), "got {text:?}");
    }

    /// #64: a daemon that answered the request with an error of its own is
    /// running and talking to us. Both of the other notes would send the user
    /// to start something already started, and leave them with no next step.
    #[test]
    fn a_daemon_that_rejected_the_request_is_named_as_that() {
        let mut app = app_with(&["a"]);
        app.set_daemon_outage(Some(Outage::Rejected));
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains("rejected") && text.contains("retrying"),
            "got {text:?}"
        );
        assert!(
            !text.contains("unreachable") && !text.contains("not answering"),
            "the daemon answered, so the bar must not say it did not: {text:?}"
        );
    }

    /// #52: a daemon that accepted the connection and then went quiet is
    /// reachable, so the note must not say otherwise. It is a different
    /// problem with a different remedy.
    #[test]
    fn a_daemon_that_is_merely_not_answering_is_not_called_unreachable() {
        let mut app = app_with(&["a"]);
        app.set_daemon_outage(Some(Outage::NotAnswering));
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains("Docker daemon not answering") && text.contains("retrying"),
            "got {text:?}"
        );
        assert!(
            !text.contains("unreachable"),
            "the connection was made, so the bar must not say it was not: {text:?}"
        );
    }

    #[test]
    fn the_note_goes_away_when_the_daemon_answers_again() {
        let mut app = app_with(&["a"]);
        app.set_daemon_outage(Some(Outage::Unreachable));
        app.set_daemon_outage(None);
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            !text.contains("Docker daemon"),
            "recovery must clear the note, got {text:?}"
        );
    }

    /// A persisted filter is a reminder of something the user typed and can
    /// retype; an outage is news. The outage takes the slot.
    #[test]
    fn an_unreachable_daemon_outranks_a_persisted_filter() {
        let mut app = app_with(&["api", "worker"]);
        for code in [
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyCode::Enter,
        ] {
            app.handle_key(
                crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
                std::time::Instant::now(),
            );
        }
        assert_eq!(app.filter().state(), FilterState::Persisted);
        app.set_daemon_outage(Some(Outage::Unreachable));
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("Docker daemon unreachable"), "got {text:?}");
        assert!(
            !text.contains("hidden"),
            "the slot holds one message, not both: {text:?}"
        );
    }

    /// The countdown keeps the slot: it is the only warning that the app is
    /// about to close itself, and it expires on its own in seconds.
    #[test]
    fn the_auto_exit_countdown_outranks_the_daemon_note() {
        let mut app = App::new("demo", &Config::default());
        app.set_services(
            ["a"]
                .iter()
                .map(|n| Service {
                    name: n.to_string(),
                    replica: 1,
                    status: ServiceStatus::Success,
                    health: Health::None,
                    exit_code: Some(0),
                    started_at: None,
                    finished_at: None,
                })
                .collect(),
        );
        app.set_daemon_outage(Some(Outage::Unreachable));
        // Only meaningful if a countdown is actually running.
        assert!(app.countdown_remaining().is_some());
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("closing in"), "got {text:?}");
        assert!(
            !text.contains("Docker daemon"),
            "the slot holds one message, not both: {text:?}"
        );
    }

    #[test]
    fn a_transient_message_outranks_the_filter() {
        let mut app = app_with(&["a"]);
        app.set_status_message("Output copied");
        let text: String = context_line(&app)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "Output copied");
    }
}
