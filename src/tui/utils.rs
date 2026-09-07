//! Status styling, duration formatting and list ordering.
//!
//! Ported from nx `packages/nx/src/native/tui/utils.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)

use crate::model::{Service, ServiceStatus};
use crate::tui::theme::THEME;
use ratatui::style::Style;
use std::collections::{HashMap, HashSet};

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

/// Reduces containers sharing a `(name, replica)` identity to a single service.
///
/// `docker compose up` creates a replacement container *before* it stops and
/// destroys the one it is replacing, so for as long as the original takes to
/// shut down -- its stop grace period, ten seconds by default and often longer
/// -- two containers carry the same `service` and `container-number` labels.
/// Neither is a one-off or a hook, so both reach the service list.
///
/// They cannot honestly be shown as two rows. `(name, replica)` is the identity
/// the log buffer, the pane pin and the selection all key on, so a second row
/// claims a distinction none of them can act on: the same label on both, the
/// same pane indicator on both, and a pair tied on `(category, name, replica)`
/// whose order then falls through to the daemon's list order and can swap from
/// one refresh to the next.
///
/// The survivor is the live member if there is one, and otherwise whichever
/// started later -- so a running original outlives its not-yet-started
/// replacement, and a live container is never dropped for a dead sibling that
/// happens to carry a later start time. When neither is live -- an exited
/// original beside its created replacement -- `None` sorts below any timestamp,
/// so the replacement still loses. Two containers compose has created but
/// not yet started render identically -- same status, no exit code, no uptime --
/// so which of those survives is not observable; any other tie keeps the one the
/// daemon listed last, arbitrarily.
///
/// A group with *two or more* live members is left alone entirely. Compose
/// destroys the original before it starts the replacement, so a recreate group
/// never has two live members, and one that does is a collision this pass did
/// not come for: a compose that omits `container-number` on a scaled service
/// (#51) defaults every replica to `1`, and collapsing there would leave one row
/// while `LogSupervisor`, which derives the replica index independently, went on
/// streaming all of them into it. The extra rows are the only sign in the UI
/// that the extra containers exist, so they stay.
pub fn collapse_replaced_containers(services: &mut Vec<Service>) {
    let mut groups: HashMap<(&str, u32), Vec<usize>> = HashMap::new();
    for (idx, service) in services.iter().enumerate() {
        groups
            .entry((service.name.as_str(), service.replica))
            .or_default()
            .push(idx);
    }

    let mut dropped: HashSet<usize> = HashSet::new();
    for members in groups.values() {
        if members.len() < 2 {
            continue;
        }
        if members
            .iter()
            .filter(|idx| is_live(services[**idx].status))
            .count()
            > 1
        {
            continue;
        }
        // Live first, so a live container is never dropped for a dead sibling
        // that happens to have started later. `max_by_key` yields the last of
        // several equal maxima, and the tuple ties only when both halves do, so
        // a genuine tie resolves to the container the daemon listed last.
        let survivor = members
            .iter()
            .copied()
            .max_by_key(|idx| (is_live(services[*idx].status), services[*idx].started_at))
            .expect("a group with two or more members is not empty");
        dropped.extend(members.iter().copied().filter(|idx| *idx != survivor));
    }

    if dropped.is_empty() {
        return;
    }
    // `retain` visits in order, which is what lets a positional predicate work.
    let mut idx = 0;
    services.retain(|_| {
        let keep = !dropped.contains(&idx);
        idx += 1;
        keep
    });
}

/// Whether a container is still doing work, so cannot be one compose has
/// finished with. Matches the statuses `sort_category` groups as active.
fn is_live(status: ServiceStatus) -> bool {
    matches!(status, ServiceStatus::Running | ServiceStatus::Unhealthy)
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

    /// A container compose has created but not yet started: no start time, so
    /// nothing to report in the uptime column.
    fn not_started(name: &str, replica: u32) -> Service {
        Service {
            replica,
            status: ServiceStatus::NotStarted,
            started_at: None,
            ..timed(name, ServiceStatus::NotStarted, 0, None)
        }
    }

    #[test]
    fn a_replacement_container_collapses_onto_the_one_it_replaces() {
        let mut services = vec![
            not_started("api", 1),
            timed("api", ServiceStatus::Running, 5, None),
        ];
        collapse_replaced_containers(&mut services);
        assert_eq!(services.len(), 1, "one container identity, one service");
        assert_eq!(
            services[0].status,
            ServiceStatus::Running,
            "the container actually serving traffic must be the survivor"
        );
    }

    #[test]
    fn the_survivor_does_not_depend_on_list_order_when_only_one_has_started() {
        let pair = vec![
            not_started("api", 1),
            timed("api", ServiceStatus::Running, 5, None),
        ];
        let survivor = |mut v: Vec<Service>| {
            collapse_replaced_containers(&mut v);
            v.into_iter().map(|s| s.status).collect::<Vec<_>>()
        };
        let mut reversed = pair.clone();
        reversed.reverse();
        assert_eq!(
            survivor(pair),
            survivor(reversed),
            "the daemon does not promise an order, so it must not pick the row"
        );
    }

    #[test]
    fn a_running_container_outlives_the_one_it_replaced() {
        // Once the original has exited but not yet been destroyed, the pair is
        // two started containers rather than a started and a created one.
        let mut services = vec![
            timed("api", ServiceStatus::Success, 5, Some(9)),
            timed("api", ServiceStatus::Running, 7, None),
        ];
        collapse_replaced_containers(&mut services);
        assert_eq!(services.len(), 1, "one container identity, one service");
        assert_eq!(
            services[0].status,
            ServiceStatus::Running,
            "the container actually serving traffic must be the survivor"
        );
    }

    #[test]
    fn the_live_member_of_three_colliding_containers_wins() {
        // A second `compose up` inside the first one's grace period stacks
        // three, which is where picking the survivor differs from picking an
        // endpoint of the list.
        for order in [[0usize, 1, 2], [2, 0, 1], [1, 2, 0], [2, 1, 0]] {
            let all = [
                not_started("api", 1),
                timed("api", ServiceStatus::Success, 5, Some(6)),
                timed("api", ServiceStatus::Running, 9, None),
            ];
            let mut services: Vec<Service> = order.iter().map(|i| all[*i].clone()).collect();
            collapse_replaced_containers(&mut services);
            assert_eq!(services.len(), 1, "{order:?}");
            assert_eq!(services[0].status, ServiceStatus::Running, "{order:?}");
        }
    }

    #[test]
    fn two_live_replicas_are_both_kept() {
        // A compose that omits `container-number` on a scaled service defaults
        // every replica to 1 (#51). Two is the boundary the guard is written
        // for, and an unhealthy container is still live: collapsing either
        // would hide a container whose output still reaches the surviving row.
        let mut services = vec![
            timed("web", ServiceStatus::Running, 1, None),
            timed("web", ServiceStatus::Unhealthy, 2, None),
        ];
        collapse_replaced_containers(&mut services);
        assert_eq!(services.len(), 2, "two live containers, two rows");
    }

    #[test]
    fn an_exited_original_outlives_its_created_replacement() {
        // A second past the observed window: the original has exited but
        // compose has not destroyed it yet, and the replacement is still
        // `created`. Neither is live, so the start time is the only thing left
        // to decide -- and the row must keep the container that actually ran,
        // with its exit code, rather than its stand-in. Both orders, because
        // the daemon promises neither.
        for reversed in [false, true] {
            let mut services = vec![
                timed("api", ServiceStatus::Failure, 5, Some(9)),
                not_started("api", 1),
            ];
            if reversed {
                services.reverse();
            }
            collapse_replaced_containers(&mut services);
            assert_eq!(services.len(), 1, "reversed={reversed}");
            assert_eq!(
                services[0].status,
                ServiceStatus::Failure,
                "reversed={reversed}: the container that actually ran must survive"
            );
        }
    }

    #[test]
    fn a_live_container_is_never_dropped_for_a_dead_one() {
        // The group has one live member, so the guard lets it through and the
        // survivor rule decides. Picking purely on start time would keep the
        // crashed container and lose the one still serving traffic.
        let mut services = vec![
            timed("web", ServiceStatus::Running, 1, None),
            timed("web", ServiceStatus::Failure, 5, Some(6)),
        ];
        collapse_replaced_containers(&mut services);
        assert_eq!(services.len(), 1);
        assert_eq!(
            services[0].status,
            ServiceStatus::Running,
            "the container actually serving traffic must be the survivor"
        );
    }

    #[test]
    fn distinct_replicas_of_a_scaled_service_are_all_kept() {
        // Only one replica is live, so the live-member guard does not wave this
        // through: a key that ignored the replica number would reach the
        // survivor rule and drop two rows.
        let mut services = vec![
            replica_of("web", 1, 1),
            Service {
                status: ServiceStatus::Success,
                ..replica_of("web", 2, 2)
            },
            Service {
                status: ServiceStatus::Success,
                ..replica_of("web", 3, 3)
            },
        ];
        collapse_replaced_containers(&mut services);
        assert_eq!(
            services.iter().map(|s| s.replica).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "a scaled service keeps one row per replica"
        );
    }

    #[test]
    fn services_that_never_collide_keep_their_arrival_order() {
        // `b` is the only live one, so the live-member guard does not wave this
        // through: a key that dropped the service name would collapse two
        // different services into one row.
        let mut services = vec![
            timed("b", ServiceStatus::Running, 1, None),
            timed("a", ServiceStatus::Success, 2, Some(3)),
        ];
        collapse_replaced_containers(&mut services);
        assert_eq!(
            services.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"],
            "two services are two rows, in arrival order"
        );
    }

    /// Guards a rewrite that reaches for `services[0]` before checking the
    /// length. It kills no mutation of the survivor rule and is not coverage
    /// of it.
    #[test]
    fn an_empty_list_collapses_to_nothing() {
        let mut services: Vec<Service> = Vec::new();
        collapse_replaced_containers(&mut services);
        assert!(services.is_empty());
    }
}
