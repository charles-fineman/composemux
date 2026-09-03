//! Status styling, duration formatting and list ordering.
//!
//! Ported from nx `packages/nx/src/native/tui/utils.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)

use crate::model::{Service, ServiceStatus};
use crate::tui::theme::THEME;
use ratatui::style::Style;

/// Status → foreground colour. Mirrors nx's `get_task_status_style`.
pub fn status_style(status: ServiceStatus) -> Style {
    Style::default().fg(match status {
        ServiceStatus::Success => THEME.success,
        ServiceStatus::Failure => THEME.error,
        ServiceStatus::Unhealthy => THEME.warning,
        ServiceStatus::Running => THEME.info,
        ServiceStatus::Stopped | ServiceStatus::NotStarted => THEME.secondary_fg,
    })
}

/// Human duration for the uptime column.
///
/// Follows nx's `format_duration` for sub-minute values, then extends into hours
/// and days — nx tasks finish in seconds, but containers stay up for days.
pub fn format_duration(d: chrono::Duration) -> String {
    let ms = d.num_milliseconds();
    if ms < 0 {
        return "...".to_string();
    }
    if ms < 1 {
        return "<1ms".to_string();
    }
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let total = d.num_seconds();
    if total < 3_600 {
        return format!("{}m {}s", total / 60, total % 60);
    }
    if total < 86_400 {
        return format!("{}h {}m", total / 3_600, (total % 3_600) / 60);
    }
    format!("{}d {}h", total / 86_400, (total % 86_400) / 3_600)
}

/// Sort category, lowest first. Same shape as nx's `sort_task_items`: active
/// work at the top, then failures, then finished, then not-yet-started.
///
/// Deviation from nx: `Unhealthy` sorts with the active group rather than the
/// finished one. An unhealthy container is still running, and burying it beneath
/// successful ones would hide exactly the thing worth looking at.
fn sort_category(status: ServiceStatus) -> u8 {
    match status {
        ServiceStatus::Running | ServiceStatus::Unhealthy => 0,
        ServiceStatus::Failure => 1,
        ServiceStatus::Success | ServiceStatus::Stopped => 2,
        ServiceStatus::NotStarted => 3,
    }
}

pub fn sort_services(services: &mut [Service]) {
    services.sort_by(|a, b| {
        sort_category(a.status)
            .cmp(&sort_category(b.status))
            .then_with(|| match sort_category(a.status) {
                // Active: oldest first, so the ordering is stable as things start.
                0 => a.started_at.cmp(&b.started_at),
                // Finished: most recently ended first.
                1 | 2 => b.finished_at.cmp(&a.finished_at),
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.replica.cmp(&b.replica))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn duration_matches_nx_thresholds() {
        assert_eq!(format_duration(Duration::microseconds(500)), "<1ms");
        assert_eq!(format_duration(Duration::milliseconds(470)), "470ms");
        assert_eq!(format_duration(Duration::milliseconds(13_400)), "13.4s");
        assert_eq!(format_duration(Duration::seconds(90)), "1m 30s");
    }

    #[test]
    fn duration_extends_past_an_hour() {
        assert_eq!(format_duration(Duration::seconds(7_500)), "2h 5m");
        assert_eq!(format_duration(Duration::seconds(180_000)), "2d 2h");
    }

    #[test]
    fn active_sorts_above_failed_above_finished_above_pending() {
        let mk = |n: &str, s: ServiceStatus| Service {
            name: n.to_string(),
            replica: 1,
            status: s,
            health: crate::model::Health::None,
            exit_code: None,
            started_at: None,
            finished_at: None,
        };
        let mut v = vec![
            mk("d", ServiceStatus::NotStarted),
            mk("c", ServiceStatus::Success),
            mk("b", ServiceStatus::Failure),
            mk("a", ServiceStatus::Running),
        ];
        sort_services(&mut v);
        let names: Vec<_> = v.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
    }
}
