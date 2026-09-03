//! Draws one frame.

use crate::tui::app::App;
use crate::tui::components::services_list::uptime_text;
use crate::tui::components::{countdown_popup, help_popup, log_pane, services_list, status_bar};
use crate::tui::focus::{Focus, MAX_PANES};
use crate::tui::layout_manager::{is_too_small, LayoutAreas};
use ratatui::Frame;

/// Computes this frame's geometry and the emulator size each pane needs.
pub fn layout_for(app: &App, area: ratatui::layout::Rect) -> (LayoutAreas, Vec<(usize, u16, u16)>) {
    let areas = app.layout().calculate(area, status_bar::HEIGHT);
    let sizes = areas
        .panes
        .iter()
        .enumerate()
        .map(|(i, rect)| {
            let (rows, cols) = log_pane::inner_size(*rect);
            (i, rows, cols)
        })
        .collect();
    (areas, sizes)
}

pub fn draw(app: &App, frame: &mut Frame) {
    let area = frame.area();
    let buf = frame.buffer_mut();

    if is_too_small(area) {
        services_list::render_too_small(&app.project, area, buf);
        return;
    }

    let areas = app.layout().calculate(area, status_bar::HEIGHT);

    if let Some(list_area) = areas.service_list {
        services_list::render(app, list_area, buf);
    }

    // The next pane `tab` would move to, so the hint appears in the right place.
    let next_tab_target = next_tab_pane(app);

    for (idx, rect) in areas.panes.iter().enumerate() {
        let key = app.pane_key(idx);
        let row = key
            .as_ref()
            .and_then(|k| app.rows().iter().find(|r| &r.key == k));
        let (title, status, uptime) = match row {
            Some(r) => (
                r.display_name.clone(),
                r.service.status,
                Some(uptime_text(r)),
            ),
            None => (
                key.as_ref()
                    .map(|k| k.name.clone())
                    .unwrap_or_else(|| "—".to_string()),
                crate::model::ServiceStatus::NotStarted,
                None,
            ),
        };
        let pane = log_pane::PaneRender {
            title: &title,
            status,
            focused: app.focus() == Focus::Pane(idx),
            store: key.as_ref().and_then(|k| app.store(k)),
            uptime,
            throbber: app.throbber(),
            tab_hint: next_tab_target == Some(idx),
        };
        log_pane::render(&pane, *rect, buf);
    }

    if let Some(bar) = areas.status_bar {
        status_bar::render(app, bar, buf);
    }

    match app.focus() {
        Focus::HelpPopup => help_popup::render(&app.project, area, buf),
        Focus::CountdownPopup => {
            if let Some(remaining) = app.countdown_remaining() {
                countdown_popup::render(app, remaining, area, buf);
            }
        }
        _ => {}
    }
}

/// Which pane `tab` would focus next, for the pane title hint.
fn next_tab_pane(app: &App) -> Option<usize> {
    if !app.has_visible_panes() {
        return None;
    }
    match app.focus() {
        Focus::ServiceList => (0..MAX_PANES).find(|i| app.pane_key(*i).is_some()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Health, Service, ServiceStatus};
    use crate::tui::app::App;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    fn service(name: &str, status: ServiceStatus) -> Service {
        Service {
            name: name.to_string(),
            replica: 1,
            status,
            health: Health::None,
            exit_code: None,
            started_at: None,
            finished_at: None,
        }
    }

    fn app() -> App {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(vec![
            service("api", ServiceStatus::Running),
            service("worker", ServiceStatus::Failure),
            service("db", ServiceStatus::Success),
        ]);
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(
            KeyEvent::new(code, KeyModifiers::NONE),
            std::time::Instant::now(),
        );
    }

    /// Renders and returns the frame as plain text lines.
    fn render_to_lines(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map(|c| c.symbol())
                            .unwrap_or(" ")
                            .to_string()
                    })
                    .collect::<String>()
            })
            .collect()
    }

    fn render_to_text(app: &App, width: u16, height: u16) -> String {
        render_to_lines(app, width, height).join("\n")
    }

    #[test]
    fn the_service_list_shows_every_service_and_the_project_badge() {
        let text = render_to_text(&app(), 120, 40);
        assert!(text.contains("api"), "missing api:\n{text}");
        assert!(text.contains("worker"));
        assert!(text.contains("db"));
        assert!(text.contains("DEMO"), "missing project badge:\n{text}");
    }

    #[test]
    fn the_status_bar_shows_key_hints() {
        let text = render_to_text(&app(), 120, 40);
        assert!(text.contains("help: ?"), "missing help hint:\n{text}");
        assert!(text.contains("quit: q"));
    }

    #[test]
    fn the_selected_row_is_marked_with_a_caret() {
        let lines = render_to_lines(&app(), 120, 40);
        let marked: Vec<_> = lines
            .iter()
            .filter(|l| l.trim_start().starts_with('>'))
            .collect();
        assert_eq!(
            marked.len(),
            1,
            "exactly one row should be marked:\n{lines:#?}"
        );
    }

    #[test]
    fn status_glyphs_reflect_each_services_state() {
        let text = render_to_text(&app(), 120, 40);
        assert!(text.contains('✖'), "failed service needs a cross:\n{text}");
        assert!(
            text.contains('✔'),
            "succeeded service needs a tick:\n{text}"
        );
    }

    #[test]
    fn a_frame_below_the_minimum_shows_only_the_too_small_notice() {
        let text = render_to_text(&app(), 30, 8);
        assert!(text.contains("Terminal too small"), "got:\n{text}");
        assert!(!text.contains("help: ?"));
    }

    #[test]
    fn the_minimum_supported_frame_still_renders_the_ui() {
        let text = render_to_text(&app(), 40, 10);
        assert!(!text.contains("Terminal too small"), "got:\n{text}");
    }

    #[test]
    fn pinning_opens_a_bordered_pane_titled_with_the_service() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains('│'), "expected a pane border:\n{text}");
        // The service appears both in the list and as the pane title.
        assert!(text.matches("api").count() >= 2, "got:\n{text}");
    }

    #[test]
    fn a_wide_frame_places_the_pane_beside_the_list() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let lines = render_to_lines(&app, 160, 40);
        // Horizontal layout: the list is a third of the width, so a border
        // appears to the right of it on the same row.
        let border_row = lines
            .iter()
            .find(|l| l.contains('┌') || l.contains('╭'))
            .expect("a pane top border");
        let x = border_row.find(['┌', '╭']).unwrap();
        assert!(x > 40, "the pane should start past the list, found at {x}");
    }

    #[test]
    fn a_narrow_frame_stacks_the_pane_below_the_list() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let lines = render_to_lines(&app, 60, 40);
        let border_y = lines
            .iter()
            .position(|l| l.contains('┌') || l.contains('╭'))
            .expect("a pane top border");
        assert!(
            border_y > 5,
            "the pane should sit below the list, at {border_y}"
        );
    }

    #[test]
    fn two_pinned_services_render_two_panes() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('2'));
        let lines = render_to_lines(&app, 160, 40);
        let borders = lines
            .iter()
            .filter(|l| l.contains('┌') || l.contains('╭'))
            .count();
        assert_eq!(borders, 2, "expected two pane tops:\n{lines:#?}");
    }

    #[test]
    fn pin_indicators_appear_next_to_pinned_services() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("[1]"), "expected a pin indicator:\n{text}");
    }

    #[test]
    fn the_help_popup_lists_the_bindings() {
        let mut app = app();
        press(&mut app, KeyCode::Char('?'));
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("Help"), "got:\n{text}");
        assert!(text.contains("Pin service to output pane 1"));
    }

    #[test]
    fn the_filter_query_is_shown_while_typing() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('a'));
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("/a"), "got:\n{text}");
        assert!(text.contains("filtered out"));
    }

    #[test]
    fn filtering_removes_non_matching_services_from_the_frame() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('w'));
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("worker"));
        assert!(!text.contains(" db "), "db should be filtered out:\n{text}");
    }

    #[test]
    fn log_output_is_visible_in_a_pane() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        app.ingest(
            crate::tui::app::ServiceKey::new("api", 1),
            b"hello from the container\r\n",
        );
        let (_, sizes) = layout_for(&app, Rect::new(0, 0, 160, 40));
        app.resize_panes(&sizes);
        let text = render_to_text(&app, 160, 40);
        assert!(text.contains("hello from the container"), "got:\n{text}");
    }

    #[test]
    fn a_pane_with_no_output_yet_says_so() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        let text = render_to_text(&app, 160, 40);
        assert!(text.contains("Waiting for output"), "got:\n{text}");
    }

    #[test]
    fn hiding_the_list_leaves_only_the_pane() {
        let mut app = app();
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('b'));
        let lines = render_to_lines(&app, 160, 40);
        let border_row = lines
            .iter()
            .find(|l| l.contains('┏') || l.contains('┌') || l.contains('╭'))
            .expect("a pane top border");
        let x = border_row.find(['┏', '┌', '╭']).unwrap();
        assert_eq!(x, 0, "with the list hidden the pane starts at column 0");
    }

    #[test]
    fn the_countdown_popup_summarises_the_exited_stack() {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(vec![
            service("api", ServiceStatus::Success),
            service("db", ServiceStatus::Success),
        ]);
        app.tick(std::time::Instant::now());
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("All services exited"), "got:\n{text}");
        assert!(text.contains("Closing in"), "got:\n{text}");
        assert!(text.contains("api") && text.contains("db"));
    }

    #[test]
    fn a_crashed_stack_shows_no_countdown_popup() {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(vec![service("api", ServiceStatus::Failure)]);
        app.tick(std::time::Instant::now());
        let text = render_to_text(&app, 120, 40);
        assert!(
            !text.contains("Closing in"),
            "a failed stack must stay open:\n{text}"
        );
    }

    #[test]
    fn the_status_bar_warns_while_the_countdown_runs() {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(vec![service("api", ServiceStatus::Success)]);
        app.tick(std::time::Instant::now());
        // Wide enough that the middle slot isn't truncated.
        let lines = render_to_lines(&app, 200, 40);
        let bar = lines.last().expect("a status bar row");
        assert!(
            bar.contains("All services exited") && bar.contains("any key cancels"),
            "status bar was: {bar:?}"
        );
    }

    #[test]
    fn an_empty_project_renders_without_panicking() {
        let cfg = Config::default();
        let app = App::new("empty", &cfg);
        let text = render_to_text(&app, 120, 40);
        assert!(text.contains("EMPTY"));
    }

    #[test]
    fn rendering_is_stable_across_a_range_of_sizes() {
        // Guards against panics from arithmetic on very small or odd frames.
        for (w, h) in [(40, 10), (41, 11), (60, 50), (80, 24), (200, 60), (120, 12)] {
            let mut app = app();
            press(&mut app, KeyCode::Char('1'));
            press(&mut app, KeyCode::Char('j'));
            press(&mut app, KeyCode::Char('2'));
            let _ = render_to_lines(&app, w, h);
        }
    }
}
