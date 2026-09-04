//! The service list (nx's task list).
//!
//! Ported from nx `packages/nx/src/native/tui/components/tasks_list.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)
//!
//! Column widths and the borderless, background-free selection are nx's; only
//! the two right-hand columns change meaning (cache/duration become
//! health/uptime).

use crate::model::ServiceStatus;
use crate::tui::app::App;
use crate::tui::focus::Focus;
use crate::tui::status_icons::status_char;
use crate::tui::theme::THEME;
use crate::tui::utils::{format_duration, status_style};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget,
    Table, TableState, Widget,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const STATUS_ICON_WIDTH: u16 = 6;
const HEALTH_COLUMN_WIDTH: u16 = 6;
const UPTIME_COLUMN_WIDTH: u16 = 10;
const COLUMN_SEPARATOR_WIDTH: u16 = 1;
/// A column reserved for the scrollbar so it never sits on top of a value.
const SCROLLBAR_WIDTH: u16 = 1;
/// The header line plus the blank row under it.
const HEADER_ROWS: u16 = 3;
/// Longest project name shown in the badge before it is elided, in cells.
const BADGE_MAX_WIDTH: u16 = 24;
const TASK_NAME_RESERVED_MIN_WIDTH: u16 = 19;
const TASK_NAME_LAYOUT_THRESHOLD: u16 = 30;

/// Which optional columns fit at this width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnVisibility {
    pub health: bool,
    pub uptime: bool,
}

/// Drops the right-hand columns as the list narrows, longest-first, so the
/// service name always stays readable.
pub fn column_visibility(width: u16, longest_name: u16) -> ColumnVisibility {
    let base = STATUS_ICON_WIDTH + COLUMN_SEPARATOR_WIDTH;
    let name_need = TASK_NAME_RESERVED_MIN_WIDTH.max(longest_name.min(TASK_NAME_LAYOUT_THRESHOLD));
    let remaining = width.saturating_sub(base).saturating_sub(name_need);

    let both = (UPTIME_COLUMN_WIDTH + COLUMN_SEPARATOR_WIDTH)
        + (HEALTH_COLUMN_WIDTH + COLUMN_SEPARATOR_WIDTH);
    if remaining >= both {
        ColumnVisibility {
            health: true,
            uptime: true,
        }
    } else if remaining >= UPTIME_COLUMN_WIDTH + COLUMN_SEPARATOR_WIDTH {
        ColumnVisibility {
            health: false,
            uptime: true,
        }
    } else {
        ColumnVisibility {
            health: false,
            uptime: false,
        }
    }
}

/// Splits the pane into the header line and the table body beneath it.
///
/// Every row count comes from here. A second, parallel calculation of the
/// body height is what let a list of exactly seventeen services draw a
/// scrollbar in a body that had room for all seventeen.
fn header_and_body(area: Rect) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(HEADER_ROWS), Constraint::Fill(1)])
        .split(area);
    (chunks[0], chunks[1])
}

pub fn render(app: &App, area: Rect, buf: &mut ratatui::buffer::Buffer) {
    let focused = app.focus() == Focus::ServiceList;

    // The header is drawn above the table rather than as its first row, so the
    // project badge is bounded by the pane rather than by the status-icon
    // column it happened to share.
    //
    // The vertical split comes first so the header keeps the full pane width:
    // reserving the scrollbar column before this point elided a project name
    // that had room to be spelled out, on a line the scrollbar never reaches.
    let (header, body) = header_and_body(area);
    render_header(app, focused, header, buf);

    // The scrollbar gets a column of its own. Drawing it over the table meant
    // it ate the last character of the rightmost value, turning `683ms` into
    // `683m` -- a wrong number that reads like a right one.
    let needs_scrollbar = scrollbar_needed(app, body);
    let table_area = Rect {
        width: body
            .width
            .saturating_sub(if needs_scrollbar { SCROLLBAR_WIDTH } else { 0 }),
        ..body
    };

    let longest = app
        .rows()
        .iter()
        .map(|r| r.display_name.len() as u16)
        .max()
        .unwrap_or(0);
    let columns = column_visibility(table_area.width, longest);

    let rows: Vec<Row> = app
        .rows()
        .iter()
        .enumerate()
        .map(|(i, row)| build_row(app, i, row, columns))
        .collect();

    let mut constraints = vec![Constraint::Length(STATUS_ICON_WIDTH), Constraint::Fill(1)];
    if columns.health {
        constraints.push(Constraint::Length(HEALTH_COLUMN_WIDTH));
    }
    if columns.uptime {
        constraints.push(Constraint::Length(UPTIME_COLUMN_WIDTH));
    }

    let base = if focused {
        Style::default().fg(THEME.secondary_fg)
    } else {
        Style::default()
            .fg(THEME.secondary_fg)
            .add_modifier(Modifier::DIM)
    };

    let table = Table::new(rows, constraints)
        .block(Block::default())
        .style(base);

    let mut state = TableState::default().with_selected(Some(app.selected_index()));
    StatefulWidget::render(table, table_area, buf, &mut state);

    if needs_scrollbar {
        render_scrollbar(app, body, buf, focused);
    }
}

/// Draws the project badge and run summary above the table.
///
/// Rendered on its own line rather than as the table's header row: as a row its
/// first cell was bound by the status-icon column, which clipped anything past
/// about five characters of the project name.
fn render_header(app: &App, focused: bool, area: Rect, buf: &mut ratatui::buffer::Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Green once everything succeeded, red if anything failed, accent while
    // work is still going.
    let any_failed = app
        .rows()
        .iter()
        .any(|r| r.service.status == ServiceStatus::Failure);
    let all_done =
        !app.rows().is_empty() && app.rows().iter().all(|r| r.service.status.is_finished());
    let badge_colour = if all_done {
        if any_failed {
            THEME.error
        } else {
            THEME.success
        }
    } else {
        THEME.info
    };

    let title_style = if focused {
        Style::default().fg(THEME.secondary_fg)
    } else {
        Style::default()
            .fg(THEME.secondary_fg)
            .add_modifier(Modifier::DIM)
    };
    let running = app
        .rows()
        .iter()
        .filter(|r| r.service.status == ServiceStatus::Running)
        .count();

    let badge = truncate_badge(&app.project, area.width);
    let mut spans = vec![Span::styled(
        badge,
        Style::reset()
            .add_modifier(Modifier::BOLD)
            .bg(badge_colour)
            .fg(THEME.primary_fg),
    )];
    let summary = format!("  {running}/{} running", app.rows().len());
    if (spans[0].content.width() + summary.width()) as u16 <= area.width {
        spans.push(Span::styled(summary, title_style));
    }

    let header = Rect { height: 1, ..area };
    Widget::render(Paragraph::new(Line::from(spans)), header, buf);
}

/// The project badge, trimmed to fit rather than clipped mid-render.
///
/// `available` counts terminal cells, so the name is measured in cells too: a
/// character is not a column, and counting characters let a name of wide
/// glyphs build a badge twice the width it was budgeted, which `Paragraph`
/// then clipped -- the mid-render truncation this exists to avoid.
fn truncate_badge(project: &str, available: u16) -> String {
    let name = project.to_uppercase();
    // Two spaces of padding, and leave room for at least part of the summary.
    let budget = available.saturating_sub(2).min(BADGE_MAX_WIDTH) as usize;
    if budget == 0 {
        return String::new();
    }
    if name.width() <= budget {
        return format!(" {name} ");
    }
    // Truncate on grapheme boundaries so a combining mark is never orphaned
    // from the character it modifies, and a wide glyph is dropped whole
    // rather than left half-drawn.
    let room = budget.saturating_sub('\u{2026}'.width().unwrap_or(1));
    let mut kept = String::new();
    let mut used = 0usize;
    for grapheme in name.graphemes(true) {
        let w = grapheme.width();
        if used + w > room {
            break;
        }
        kept.push_str(grapheme);
        used += w;
    }
    format!(" {kept}\u{2026} ")
}

fn build_row<'a>(
    app: &App,
    index: usize,
    row: &crate::tui::app::Row,
    columns: ColumnVisibility,
) -> Row<'a> {
    let selected = index == app.selected_index();
    let status = row.service.status;

    // Selection is a marker plus bold, never a background highlight.
    let mut spans = vec![Span::raw(if selected { ">" } else { " " }), Span::raw(" ")];
    spans.push(Span::styled(
        format!("{}   ", status_char(status, app.throbber())),
        status_style(status).add_modifier(Modifier::BOLD),
    ));

    let name_style = if selected {
        Style::default()
            .fg(THEME.primary_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let mut name_spans = vec![Span::styled(row.display_name.clone(), name_style)];
    for idx in app.pane_indicators(&row.key) {
        name_spans.push(Span::styled(
            format!(" [{}]", idx + 1),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    let mut cells = vec![
        Cell::from(Line::from(spans)),
        Cell::from(Line::from(name_spans)),
    ];

    if columns.health {
        let label = row.service.health.label();
        let style = health_style(row.service.health, selected);
        cells.push(Cell::from(
            Line::from(Span::styled(label, style)).right_aligned(),
        ));
    }
    if columns.uptime {
        let text = uptime_text(row);
        let mut style = Style::default();
        if selected {
            style = style.add_modifier(Modifier::BOLD);
        }
        if text == "..." {
            style = style.add_modifier(Modifier::DIM);
        }
        cells.push(Cell::from(
            Line::from(Span::styled(text, style)).right_aligned(),
        ));
    }

    Row::new(cells)
}

fn health_style(health: crate::model::Health, selected: bool) -> Style {
    let mut style = match health {
        crate::model::Health::Unhealthy => Style::default().fg(THEME.error),
        crate::model::Health::Healthy => Style::default().fg(THEME.success),
        crate::model::Health::Starting => Style::default().fg(THEME.warning),
        crate::model::Health::None => Style::default().add_modifier(Modifier::DIM),
    };
    if selected {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

/// Uptime for a live service, or how long a finished one ran plus its code.
pub fn uptime_text(row: &crate::tui::app::Row) -> String {
    match row.service.status {
        ServiceStatus::NotStarted => "...".to_string(),
        _ => match row.service.duration() {
            Some(d) => {
                if let Some(code) = row.service.exit_code {
                    if code != 0 {
                        return format!("exit {code}");
                    }
                }
                format_duration(d)
            }
            None => "...".to_string(),
        },
    }
}

/// Whether the list is longer than the space available for it.
/// Whether the list overflows the table body it is drawn into.
///
/// `body` is the table's own rect, not the whole pane, so an exactly-fitting
/// list is not given a scrollbar it does not need.
fn scrollbar_needed(app: &App, body: Rect) -> bool {
    body.width >= 2 && app.rows().len() > body.height as usize
}

/// Draws the scrollbar in the column reserved for it alongside the table body.
fn render_scrollbar(app: &App, body: Rect, buf: &mut ratatui::buffer::Buffer, focused: bool) {
    let total = app.rows().len();
    let viewport = body.height as usize;
    if total <= viewport || body.width < 2 {
        return;
    }
    let mut state = ScrollbarState::default()
        .content_length(total.saturating_sub(viewport))
        .viewport_content_length(viewport)
        .position(app.selected_index().min(total.saturating_sub(viewport)));
    let style = if focused {
        Style::default().fg(THEME.info)
    } else {
        Style::default().fg(THEME.info).add_modifier(Modifier::DIM)
    };
    let bar = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .style(style);
    StatefulWidget::render(bar, body, buf, &mut state);
}

/// Message shown when the frame is too small to draw the UI.
pub fn render_too_small(project: &str, area: Rect, buf: &mut ratatui::buffer::Buffer) {
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", project.to_uppercase()),
            Style::reset()
                .add_modifier(Modifier::BOLD)
                .bg(THEME.error)
                .fg(THEME.primary_fg),
        ),
        Span::raw(" Terminal too small "),
    ]);
    let y = area.y + area.height / 2;
    let text_width = line.width() as u16;
    let x = area.x + area.width.saturating_sub(text_width) / 2;
    Widget::render(
        ratatui::widgets::Paragraph::new(line),
        Rect::new(x, y, text_width.min(area.width), 1),
        buf,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Health, Service};

    /// An app listing `count` running services, named so they sort readably.
    fn app_with_services(count: usize) -> App {
        let cfg = Config::default();
        let mut app = App::new("demo", &cfg);
        app.set_services(
            (0..count)
                .map(|i| Service {
                    name: format!("svc-{i:02}"),
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

    /// The list rendered into a buffer of the given size.
    fn render_to_buffer(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        render(app, area, &mut buf);
        buf
    }

    #[test]
    fn wide_lists_show_both_optional_columns() {
        let v = column_visibility(60, 10);
        assert!(v.health && v.uptime);
    }

    #[test]
    fn uptime_survives_longer_than_health_as_width_shrinks() {
        let v = column_visibility(40, 10);
        assert!(v.uptime, "uptime is the more useful of the two");
        assert!(!v.health);
    }

    #[test]
    fn very_narrow_lists_drop_both_columns() {
        let v = column_visibility(26, 10);
        assert!(!v.health && !v.uptime);
    }

    #[test]
    fn a_long_service_name_is_capped_when_reserving_space() {
        // A 200-char name must not starve the columns entirely; the reservation
        // is capped at the layout threshold.
        let v = column_visibility(80, 200);
        assert!(v.uptime && v.health);
    }

    #[test]
    fn a_long_project_name_is_not_clipped_to_the_status_column() {
        // The badge used to sit in the table's status-icon column, so
        // "digital-university" rendered as "DIGIT" no matter how wide the pane.
        let badge = truncate_badge("digital-university", 60);
        assert!(
            badge.contains("DIGITAL-UNIVERSITY"),
            "expected the whole name, got {badge:?}"
        );
    }

    #[test]
    fn a_very_long_project_name_is_elided_rather_than_clipped() {
        let badge = truncate_badge(&"x".repeat(80), 60);
        assert!(
            badge.ends_with("\u{2026} "),
            "should end in an ellipsis: {badge:?}"
        );
        assert!(badge.chars().count() <= BADGE_MAX_WIDTH as usize + 2);
    }

    #[test]
    fn a_narrow_pane_still_produces_a_usable_badge() {
        for width in [0u16, 1, 2, 3, 8, 40] {
            let badge = truncate_badge("platform", width);
            assert!(
                badge.chars().count() <= width.max(2) as usize + 2,
                "badge {badge:?} overflows a {width}-column pane"
            );
        }
    }

    #[test]
    fn the_body_is_the_pane_less_the_header() {
        for height in 0..40u16 {
            let (header, body) = header_and_body(Rect::new(0, 0, 40, height));
            assert_eq!(
                header.height + body.height,
                height,
                "the split lost rows at height {height}"
            );
            assert_eq!(
                body.height,
                height.saturating_sub(HEADER_ROWS),
                "body height at {height}"
            );
        }
    }

    /// A list that fits exactly must not be given a scrollbar: the old row
    /// count was one short of the body, so seventeen services in a
    /// seventeen-row body scrolled for no reason.
    #[test]
    fn a_list_that_exactly_fills_the_body_gets_no_scrollbar() {
        let area = Rect::new(0, 0, 40, 20);
        let (_, body) = header_and_body(area);
        assert_eq!(body.height, 17);

        let exact = app_with_services(body.height as usize);
        assert!(
            !scrollbar_needed(&exact, body),
            "{} services fit in {} rows",
            body.height,
            body.height
        );

        let one_too_many = app_with_services(body.height as usize + 1);
        assert!(scrollbar_needed(&one_too_many, body));
    }

    /// The scrollbar column must not shorten the header, which is drawn on a
    /// row the scrollbar never occupies.
    /// The scrollbar column must not shorten the header, which sits on a row
    /// the scrollbar never occupies.
    ///
    /// Sized to the boundary deliberately: the name is exactly as wide as the
    /// pane allows, so losing a single column to the scrollbar is the
    /// difference between spelling it out and eliding it.
    #[test]
    fn the_scrollbar_does_not_narrow_the_header() {
        let width = BADGE_MAX_WIDTH + 2;
        let project = "a".repeat(BADGE_MAX_WIDTH as usize);
        assert!(
            truncate_badge(&project, width - 1).contains('\u{2026}'),
            "the test is not at the boundary: one column narrower must elide"
        );

        // Enough services to force a scrollbar into the pane.
        let mut app = app_with_services(60);
        app.project = project.clone();
        let buf = render_to_buffer(&app, width, 20);
        let header: String = (0..width).map(|x| buf[(x, 0)].symbol()).collect();

        assert!(
            !header.contains('\u{2026}'),
            "header {header:?} elided a name the pane had room for"
        );
        assert!(header.contains(&project.to_uppercase()));
    }

    #[test]
    fn the_badge_is_bounded_by_cells_not_characters() {
        // Five double-width glyphs are ten cells, not five.
        let badge = truncate_badge("\u{754c}\u{754c}\u{754c}\u{754c}\u{754c}", 8);
        assert!(
            badge.width() <= 8,
            "badge {badge:?} is {} cells wide in an 8-cell budget",
            badge.width()
        );
    }

    #[test]
    fn truncation_counts_a_combining_sequence_as_one_cell() {
        // Each `e` carries a combining acute, so twelve letters are 24 chars
        // but only 12 cells. Counting chars spent the budget twice over and
        // truncated to 7 of the 10 cells available.
        let name = "e\u{301}".repeat(12);
        let badge = truncate_badge(&name, 10);
        assert_eq!(
            badge.width(),
            10,
            "badge {badge:?} used {} of its 10 cells",
            badge.width()
        );
    }
}
