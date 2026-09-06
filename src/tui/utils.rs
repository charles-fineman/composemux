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
/// by scheduler noise -- the five containers measured for #22 landed within
/// 1.6ms of each other -- and ordering on that dealt a fresh permutation of the
/// whole sidebar on every `compose up`, `restart` and `down`. Name is free,
/// already fetched, and is what `docker compose ps` prints, so row positions
/// stay put and a fixed keystroke sequence reaches the same service across
/// restarts.
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
    use proptest::prelude::*;

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

    /// Every `ServiceStatus`, kept in one place next to the tests that sample
    /// it: the property test draws from this rather than from an integer range,
    /// so widening its coverage to a new variant means adding it here.
    const ALL_STATUSES: [ServiceStatus; 6] = [
        ServiceStatus::Running,
        ServiceStatus::Success,
        ServiceStatus::Failure,
        ServiceStatus::Unhealthy,
        ServiceStatus::Stopped,
        ServiceStatus::NotStarted,
    ];

    /// Like `timed`, for the scaled case where one name spans several rows.
    fn replica_of(name: &str, replica: u32, started_ms: i64) -> Service {
        Service {
            replica,
            ..timed(name, ServiceStatus::Running, started_ms, None)
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
                    // Hand the sort a list that is not already in the expected
                    // order, so a comparator that did nothing could not pass on
                    // `sort_by`'s stability alone.
                    .rev()
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
        let mk = |n: &str, s: ServiceStatus| timed(n, s, 0, None);
        // Names run backwards against the category ranks, so sorting by name
        // alone cannot reproduce this order -- otherwise the test would still
        // pass with the grouping deleted. Two services per category also pin
        // down that the name comparison applies *within* a group.
        let mut v = vec![
            mk("api", ServiceStatus::NotStarted),
            mk("nginx", ServiceStatus::Success),
            mk("web", ServiceStatus::Running),
            mk("cache", ServiceStatus::NotStarted),
            mk("redis", ServiceStatus::Failure),
            mk("postgres", ServiceStatus::Stopped),
            mk("sidecar", ServiceStatus::Unhealthy),
        ];
        sort_services(&mut v);
        let names: Vec<_> = v.iter().map(|s| s.name.as_str()).collect();
        // Active first -- sidecar is Unhealthy, which groups with Running by
        // design -- then failed, then finished (Success and Stopped share a
        // category), then never started; alphabetical inside each group.
        assert_eq!(
            names,
            ["sidecar", "web", "redis", "nginx", "postgres", "api", "cache"]
        );
    }

    #[test]
    fn replicas_of_one_service_are_ordered_by_index() {
        // The higher replica started first, so the old start-time tie-break put
        // api-2 above api-1. The second name keeps both axes in play: name has
        // to outrank replica, or a scaled service's rows stop being adjacent.
        let mut v = vec![
            replica_of("api", 2, 1_000),
            replica_of("web", 1, 2_000),
            replica_of("api", 1, 3_000),
        ];
        sort_services(&mut v);
        let keys: Vec<_> = v.iter().map(|s| (s.name.as_str(), s.replica)).collect();
        assert_eq!(keys, [("api", 1), ("api", 2), ("web", 1)]);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        /// The property #22 was filed against, stated directly: what the user
        /// sees must be a function of the services alone. Container timestamps
        /// and the order the daemon happens to return rows in are both outside
        /// the user's control, so re-dealing either must change nothing.
        #[test]
        fn neither_timestamps_nor_arrival_order_reach_the_screen(
            (statuses, stamps_a, stamps_b) in (1usize..8).prop_flat_map(|n| {
                // All three are drawn at the same length, so widening the row
                // count later cannot leave `deal` indexing off the end.
                (
                    prop::collection::vec(prop::sample::select(&ALL_STATUSES[..]), n),
                    prop::collection::vec(0i64..1_000, n),
                    prop::collection::vec(0i64..1_000, n),
                )
            }),
        ) {
            // One distinct name per row, so (name, replica) is unique and the
            // ordering is total; only the timestamps and the deal differ.
            let deal = |stamps: &[i64], reversed: bool| {
                let mut v: Vec<Service> = statuses
                    .iter()
                    .enumerate()
                    .map(|(i, &s)| timed(&format!("svc{i}"), s, stamps[i], Some(stamps[i])))
                    .collect();
                if reversed {
                    v.reverse();
                }
                sort_services(&mut v);
                v.into_iter().map(|s| s.name).collect::<Vec<_>>()
            };
            prop_assert_eq!(deal(&stamps_a, false), deal(&stamps_b, true));
        }
    }
}
