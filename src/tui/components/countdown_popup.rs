#![allow(clippy::missing_docs_in_private_items)] // 1 left to document
//! Auto-exit countdown and the session summary.
//!
//! Ported from nx `packages/nx/src/native/tui/components/countdown_popup.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)
//!
//! Nx shows a performance report when a run finishes; the equivalent here is a
//! summary of how each service ended, which is what you want in front of you
//! when a stack has just fallen over.

use crate::model::ServiceStatus;
use crate::tui::app::App;
use crate::tui::components::help_popup::centered;
use crate::tui::components::services_list::uptime_text;
use crate::tui::status_icons::status_char;
use crate::tui::theme::THEME;
use crate::tui::utils::status_style;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Widget};

pub fn render(app: &App, remaining: u64, area: Rect, buf: &mut Buffer) {
    let popup = centered(70, 60, area);
    Widget::render(Clear, popup, buf);

    let any_failed = app
        .rows()
        .iter()
        .any(|r| r.service.status == ServiceStatus::Failure);
    let accent = if any_failed {
        THEME.error
    } else {
        THEME.success
    };

    let title = Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!(" {} ", app.project.to_uppercase()),
            Style::reset()
                .add_modifier(Modifier::BOLD)
                .bg(accent)
                .fg(THEME.primary_fg),
        ),
        Span::styled(
            "  All services exited  ",
            Style::default().fg(THEME.primary_fg),
        ),
    ]);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(accent))
        .padding(Padding::proportional(1));

    let mut lines: Vec<Line> = app
        .rows()
        .iter()
        .map(|row| {
            let status = row.service.status;
            Line::from(vec![
                Span::styled(
                    format!(" {} ", status_char(status, 0)),
                    status_style(status).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<24}", row.display_name),
                    Style::default().fg(THEME.primary_fg),
                ),
                Span::styled(uptime_text(row), Style::default().fg(THEME.secondary_fg)),
            ])
        })
        .collect();

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Closing in ", Style::default().fg(THEME.secondary_fg)),
        Span::styled(
            format!("{remaining}s"),
            Style::default().fg(THEME.info).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " - press any key to stay, q to quit now",
            Style::default().fg(THEME.secondary_fg),
        ),
    ]));

    Widget::render(Paragraph::new(lines).block(block), popup, buf);
}
