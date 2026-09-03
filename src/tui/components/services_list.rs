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
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Table,
    TableState, Widget,
};

const STATUS_ICON_WIDTH: u16 = 6;
const HEALTH_COLUMN_WIDTH: u16 = 6;
const UPTIME_COLUMN_WIDTH: u16 = 10;
const COLUMN_SEPARATOR_WIDTH: u16 = 1;
/// Header, spacing row and the blank line beneath, as in nx.
const HEADER_OVERHEAD_ROWS: u16 = 3;
const BOTTOM_PADDING_ROWS: u16 = 1;
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

/// Rows visible in the table body at this height.
pub fn viewport_height(area_height: u16) -> u16 {
    area_height.saturating_sub(HEADER_OVERHEAD_ROWS + BOTTOM_PADDING_ROWS)
}

pub fn render(app: &App, area: Rect, buf: &mut ratatui::buffer::Buffer) {
    let focused = app.focus() == Focus::ServiceList;
    let longest = app
        .rows()
        .iter()
        .map(|r| r.display_name.len() as u16)
        .max()
        .unwrap_or(0);
    let columns = column_visibility(area.width, longest);

    let header = header_row(app, focused, columns);
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
        .header(header)
        .block(Block::default())
        .style(base);

    let mut state = TableState::default().with_selected(Some(app.selected_index()));
    StatefulWidget::render(table, area, buf, &mut state);

    render_scrollbar(app, area, buf, focused);
}

fn header_row<'a>(app: &App, focused: bool, columns: ColumnVisibility) -> Row<'a> {
    // The badge turns green when everything succeeded and red if anything
    // failed, mirroring nx's run-status colouring.
    let any_failed = app
        .rows()
        .iter()
        .any(|r| r.service.status == ServiceStatus::Failure);
    let all_done =
        !app.rows().is_empty() && app.rows().iter().all(|r| r.service.status.is_finished());
    let badge_color = if all_done {
        if any_failed {
            THEME.error
        } else {
            THEME.success
        }
    } else {
        THEME.info
    };

    let badge = Span::styled(
        format!(" {} ", app.project.to_uppercase()),
        Style::reset()
            .add_modifier(Modifier::BOLD)
            .bg(badge_color)
            .fg(THEME.primary_fg),
    );

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
    let title = Span::styled(
        format!("  {running}/{} running", app.rows().len()),
        title_style,
    );

    let mut cells = vec![Cell::from(Line::from(badge)), Cell::from(Line::from(title))];
    let label = Style::default()
        .fg(badge_color)
        .add_modifier(Modifier::BOLD);
    if columns.health {
        cells.push(Cell::from(
            Line::from(Span::styled("Health", label)).right_aligned(),
        ));
    }
    if columns.uptime {
        cells.push(Cell::from(
            Line::from(Span::styled("Uptime", label)).right_aligned(),
        ));
    }
    Row::new(cells).top_margin(1).bottom_margin(1)
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

fn render_scrollbar(app: &App, area: Rect, buf: &mut ratatui::buffer::Buffer, focused: bool) {
    let total = app.rows().len();
    let viewport = viewport_height(area.height) as usize;
    if total <= viewport || area.width < 2 {
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
    let bar_area = Rect {
        y: area.y.saturating_add(2),
        height: area.height.saturating_sub(2),
        ..area
    };
    StatefulWidget::render(bar, bar_area, buf, &mut state);
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
    fn viewport_excludes_the_header_and_padding() {
        assert_eq!(viewport_height(20), 16);
        assert_eq!(viewport_height(4), 0);
        assert_eq!(viewport_height(0), 0);
    }
}
