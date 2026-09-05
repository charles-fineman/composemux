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

/// Orders the sidebar: status category first, then service name, then replica.
///
/// Deviation from nx, which breaks ties inside a category by start and finish
/// time. That reads well for nx, where tasks run as a dependency graph drains
/// and *when* a task started genuinely distinguishes it. Compose starts and
/// stops a project's containers concurrently, so their timestamps differ only
/// by scheduler noise -- five services measured here landed 1.6ms apart -- and
/// ordering on that dealt a fresh permutation of the whole sidebar on every
/// `compose up`, `restart` and `down`. Name is free, already fetched, and is
/// what `docker compose ps` prints, so `1`, `2` and the arrow keys keep
/// pointing at the same service across restarts.
pub fn sort_services(services: &mut [Service]) {
    services.sort_by(|a, b| {
        sort_category(a.status)
            .cmp(&sort_category(b.status))
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

    /// A service whose timestamps can be set, so a `compose up` race can be
    /// replayed without a daemon.
    fn timed(
        name: &str,
        status: ServiceStatus,
        started_ms: i64,
        finished_ms: Option<i64>,
    ) -> Service {
        use chrono::TimeZone;
        let at = |ms: i64| chrono::Utc.timestamp_millis_opt(ms).unwrap();
        Service {
            name: name.to_string(),
            replica: 1,
            status,
            health: crate::model::Health::None,
            exit_code: None,
            started_at: Some(at(started_ms)),
            finished_at: finished_ms.map(at),
        }
    }

    fn ordered(mut services: Vec<Service>) -> Vec<String> {
        sort_services(&mut services);
        services.into_iter().map(|s| s.name).collect()
    }

    #[test]
    fn running_services_are_ordered_by_name_not_by_when_they_started() {
        // Start times deliberately contradict alphabetical order.
        let names = ordered(vec![
            timed("charlie", ServiceStatus::Running, 1_000, None),
            timed("alpha", ServiceStatus::Running, 4_000, None),
            timed("bravo", ServiceStatus::Running, 2_000, None),
        ]);
        assert_eq!(names, ["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn exited_services_are_ordered_by_name_not_by_when_they_finished() {
        let names = ordered(vec![
            // Finish times ascend with the name, so "most recently finished
            // first" would invert the list.
            timed("bravo", ServiceStatus::Success, 0, Some(2_000)),
            timed("charlie", ServiceStatus::Success, 0, Some(3_000)),
            timed("alpha", ServiceStatus::Success, 0, Some(1_000)),
        ]);
        assert_eq!(names, ["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn restarting_a_project_does_not_reshuffle_the_sidebar() {
        // Compose starts a project's containers concurrently, so each run deals
        // a different sub-millisecond start order. Two such orders, observed
        // from the same five-service project across two `compose restart` runs,
        // must still render identically or `1` and `2` pin different services
        // each time.
        let run = |offsets: [i64; 5]| {
            let names = ["alpha", "bravo", "charlie", "delta", "echo"];
            ordered(
                names
                    .iter()
                    .zip(offsets)
                    .map(|(n, off)| timed(n, ServiceStatus::Running, 1_700_000_000_000 + off, None))
                    .collect(),
            )
        };
        // echo bravo alpha delta charlie
        let first = run([2, 1, 4, 3, 0]);
        // charlie alpha delta echo bravo
        let second = run([1, 4, 0, 2, 3]);
        assert_eq!(first, second);
        assert_eq!(first, ["alpha", "bravo", "charlie", "delta", "echo"]);
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
