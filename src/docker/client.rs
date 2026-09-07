#![allow(clippy::missing_docs_in_private_items)] // 5 left to document
//! Docker Engine API access, scoped to a single compose project.

use crate::docker::labels;
use crate::model::{Health, Service, ServiceStatus};
use anyhow::{Context, Result};
use bollard::models::{
    ContainerState, ContainerStateStatusEnum, ContainerSummary, HealthStatusEnum,
};
use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptions, ListContainersOptionsBuilder,
};
use bollard::Docker;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use std::collections::{HashMap, HashSet};

/// Inspect calls issued at once when refreshing the service list. Compose
/// projects are small, but one round-trip per container in series is noticeably
/// slow against a remote Docker context.
const INSPECT_CONCURRENCY: usize = 8;

pub struct DockerClient {
    docker: Docker,
}

impl DockerClient {
    /// Connects using the same resolution order as the docker CLI
    /// (`DOCKER_HOST`, then the platform default socket or named pipe), and
    /// negotiates an API version so we work across daemon releases.
    pub async fn connect() -> Result<Self> {
        let docker = Docker::connect_with_defaults()
            .context("could not connect to the Docker daemon")?
            .negotiate_version()
            .await
            .context("could not negotiate an API version with the Docker daemon")?;
        Ok(Self { docker })
    }

    /// The underlying bollard handle, for components that stream directly.
    pub fn raw(&self) -> &Docker {
        &self.docker
    }

    /// List options for every container carrying the compose project label,
    /// running or not.
    ///
    /// `Some(project)` narrows to that project; `None` matches any value of
    /// the label, which is how the project names themselves are discovered.
    ///
    /// One construction rather than one per caller, because
    /// [`list_container_keys`](Self::list_container_keys) is only comparable
    /// with what [`list_services`](Self::list_services) reports if the two ask
    /// the daemon the same question. Separate copies read as obviously alike
    /// and sit far enough apart to stop being so without anyone noticing.
    fn project_containers(project: Option<&str>) -> ListContainersOptions {
        let label = match project {
            Some(project) => format!("{}={}", labels::PROJECT, project),
            None => labels::PROJECT.to_string(),
        };
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![label]);
        ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build()
    }

    /// Every container belonging to `project`, one `Service` per container.
    ///
    /// One-off (`compose run`) and lifecycle-hook containers are excluded: they
    /// are transient and would otherwise churn the sidebar.
    pub async fn list_services(&self, project: &str) -> Result<Vec<Service>> {
        let summaries = self
            .docker
            .list_containers(Some(Self::project_containers(Some(project))))
            .await
            .context("could not list containers")?;

        let services = futures::stream::iter(summaries)
            .filter(|summary| futures::future::ready(!is_transient(summary)))
            .map(|summary| async move {
                let inspected = match summary.id.as_deref() {
                    Some(id) => self
                        .docker
                        .inspect_container(id, None::<InspectContainerOptions>)
                        .await
                        .ok(),
                    None => None,
                };
                build_service(&summary, inspected.as_ref().and_then(|i| i.state.as_ref()))
            })
            .buffer_unordered(INSPECT_CONCURRENCY)
            .filter_map(futures::future::ready)
            .collect()
            .await;
        Ok(services)
    }

    /// The `(service, replica)` of every container in the project.
    ///
    /// One-off (`compose run`) and lifecycle-hook containers are excluded, the
    /// same way `LogSupervisor::resync` excludes them from what it attaches
    /// to. A key that no log stream can ever be delivered under would look
    /// like a container that is alive and silent.
    ///
    /// [`list_services`](Self::list_services) inspects each container to fill
    /// in status, health and timings. A caller that only needs to know which
    /// containers exist would pay a round trip apiece for fields it throws
    /// away, so this reads the identity off the list entry's labels and stops
    /// there.
    ///
    /// Those are the same labels, filtered the same way, that
    /// `LogSupervisor::resync` uses to decide what to attach to. Deriving the
    /// two alike is what makes this answer comparable with the keys the log
    /// streams are actually delivered under.
    ///
    /// `all(true)`, so a container that has stopped but has not been removed
    /// is still reported. A key disappears from here only once the container
    /// is gone for good.
    pub async fn list_container_keys(&self, project: &str) -> Result<HashSet<(String, u32)>> {
        let summaries = self
            .docker
            .list_containers(Some(Self::project_containers(Some(project))))
            .await
            .context("could not list containers")?;

        Ok(summaries.iter().filter_map(container_key).collect())
    }

    /// Distinct compose project names visible to the daemon. Used to give a
    /// useful error when the requested project isn't running.
    pub async fn list_projects(&self) -> Result<Vec<String>> {
        let summaries = self
            .docker
            .list_containers(Some(Self::project_containers(None)))
            .await?;
        let mut names: Vec<String> = summaries
            .iter()
            .filter_map(|s| s.labels.as_ref()?.get(labels::PROJECT).cloned())
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }
}

/// The `(service, replica)` a list entry belongs to, or `None` if it is not a
/// container this tool follows.
///
/// Kept free of I/O for the same reason [`build_service`] is: the mapping is
/// the part that can be wrong, and it is worth reaching without a daemon.
///
/// Shared with `LogSupervisor::resync` rather than written twice. The
/// fallback's reclaim compares what this reports against the keys the log
/// streams are delivered under, so the two have to agree exactly: a container
/// one of them called replica 0 while the other called it 1 would look
/// departed for the whole run, and its held line would be cut in half every
/// time the reclaim came round. One definition is what makes that agreement
/// structural instead of a convention nothing checks.
pub(super) fn container_key(summary: &ContainerSummary) -> Option<(String, u32)> {
    // Rejected the way `resync` and `build_service` reject it. Neither can do
    // anything with an entry that has no id, so one reported alive here would
    // be a key no log stream is ever delivered under.
    summary.id.as_ref()?;
    let labels_map = summary.labels.as_ref()?;
    if is_transient_labels(labels_map) {
        return None;
    }
    // Compose omits the number on an unscaled service, which is replica 1 --
    // the same default `LogSupervisor::resync` applies to the same label.
    let replica = labels_map
        .get(labels::CONTAINER_NUMBER)
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    Some((labels_map.get(labels::SERVICE)?.clone(), replica))
}

/// The one daemon call the periodic service poll makes.
///
/// A trait rather than the concrete client because `main`'s refresh loop is
/// otherwise only drivable against a live daemon, and the interesting cases --
/// a poll that fails, and a poll that is accepted and never answered -- are
/// exactly the ones a live daemon will not arrange on request. This is the
/// seam a test stands in at.
pub trait ServiceSource: Send + Sync + 'static {
    /// The project's services, as [`DockerClient::list_services`] reports them.
    ///
    /// Returns a future rather than being an `async fn` so the `Send` bound can
    /// be written down: the poll runs in a spawned task.
    fn list_services(
        &self,
        project: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Service>>> + Send;
}

impl ServiceSource for DockerClient {
    fn list_services(
        &self,
        project: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Service>>> + Send {
        // The inherent method, which takes precedence over this one in path
        // resolution; `unconditional_recursion` would catch it if it did not.
        DockerClient::list_services(self, project)
    }
}

/// Builds a `Service` from a list entry and the container's inspected state.
///
/// Kept free of I/O so the status mapping can be tested directly.
///
/// The identity comes from [`container_key`] rather than a second reading of
/// the same two labels. The sidebar and the fallback's reclaim have to name a
/// container alike, and the container-number default is exactly the kind of
/// rule that drifts once it is written twice. Reusing the key also brings its
/// transient filter along, which narrows nothing in practice:
/// [`list_services`](DockerClient::list_services), the only caller, already
/// drops one-off and hook containers before this, so as not to inspect them.
fn build_service(summary: &ContainerSummary, state: Option<&ContainerState>) -> Option<Service> {
    let (name, replica) = container_key(summary)?;

    let exit_code = state.and_then(|s| s.exit_code);
    let started_at = state
        .and_then(|s| s.started_at.as_deref())
        .and_then(parse_ts);
    let finished_at = state
        .and_then(|s| s.finished_at.as_deref())
        .and_then(parse_ts);
    let health = state
        .and_then(|s| s.health.as_ref())
        .and_then(|h| h.status)
        .map(map_health)
        .unwrap_or(Health::None);

    let status = derive_status(state.and_then(|s| s.status), exit_code, health);

    Some(Service {
        name,
        replica,
        status,
        health,
        // Docker keeps the previous run's ExitCode and FinishedAt on a container
        // that has since restarted, so neither is meaningful until it finishes.
        exit_code: exit_code.filter(|_| status.is_finished()),
        started_at,
        finished_at: finished_at.filter(|_| status.is_finished()),
    })
}

fn is_transient(summary: &ContainerSummary) -> bool {
    summary.labels.as_ref().is_some_and(is_transient_labels)
}

/// Whether a container is one compose created for a one-off `run` or a
/// lifecycle hook. Both are ephemeral and would otherwise churn the sidebar.
///
/// Module-local again: `container_key` is now the only caller outside this
/// file's own `is_transient`, so `LogSupervisor::resync` no longer reaches for
/// it directly. Within this file it still has two application sites --
/// `is_transient`, which `list_services` filters through, and `container_key`.
fn is_transient_labels(labels_map: &HashMap<String, String>) -> bool {
    labels_map.get(labels::ONEOFF).is_some_and(|v| v == "True")
        || labels_map.contains_key(labels::HOOK)
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    // Docker reports a zero value for timestamps that never happened.
    if raw.starts_with("0001-01-01") {
        return None;
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn map_health(status: HealthStatusEnum) -> Health {
    match status {
        HealthStatusEnum::HEALTHY => Health::Healthy,
        HealthStatusEnum::UNHEALTHY => Health::Unhealthy,
        HealthStatusEnum::STARTING => Health::Starting,
        _ => Health::None,
    }
}

/// Maps container state to the nx-equivalent status.
///
/// An unhealthy or restarting container is surfaced as `Unhealthy` (nx's yellow
/// `Skipped` glyph) rather than as plain running, so a crash-looping service is
/// visible without opening its logs.
fn derive_status(
    state: Option<ContainerStateStatusEnum>,
    exit_code: Option<i64>,
    health: Health,
) -> ServiceStatus {
    match state {
        Some(ContainerStateStatusEnum::RUNNING) => match health {
            Health::Unhealthy => ServiceStatus::Unhealthy,
            _ => ServiceStatus::Running,
        },
        Some(ContainerStateStatusEnum::RESTARTING) => ServiceStatus::Unhealthy,
        Some(ContainerStateStatusEnum::PAUSED)
        | Some(ContainerStateStatusEnum::REMOVING)
        | Some(ContainerStateStatusEnum::STOPPING) => ServiceStatus::Stopped,
        Some(ContainerStateStatusEnum::EXITED) | Some(ContainerStateStatusEnum::DEAD) => {
            match exit_code {
                Some(0) => ServiceStatus::Success,
                _ => ServiceStatus::Failure,
            }
        }
        Some(ContainerStateStatusEnum::CREATED) => ServiceStatus::NotStarted,
        _ => ServiceStatus::NotStarted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(labels_pairs: &[(&str, &str)], with_id: bool) -> ContainerSummary {
        ContainerSummary {
            id: with_id.then(|| "container-id".to_string()),
            labels: Some(
                labels_pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    fn compose_summary() -> ContainerSummary {
        summary(
            &[
                (labels::PROJECT, "demo"),
                (labels::SERVICE, "api"),
                (labels::CONTAINER_NUMBER, "2"),
            ],
            true,
        )
    }

    fn state(status: ContainerStateStatusEnum, exit_code: Option<i64>) -> ContainerState {
        ContainerState {
            status: Some(status),
            exit_code,
            started_at: Some("2026-01-01T00:00:00Z".to_string()),
            finished_at: Some("2026-01-02T00:00:00Z".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn build_service_reads_the_compose_labels() {
        let svc = build_service(
            &compose_summary(),
            Some(&state(ContainerStateStatusEnum::RUNNING, None)),
        )
        .expect("a compose container yields a service");
        assert_eq!(svc.name, "api");
        assert_eq!(svc.replica, 2);
        assert_eq!(svc.status, ServiceStatus::Running);
    }

    #[test]
    fn a_missing_container_number_defaults_to_the_first_replica() {
        let s = summary(&[(labels::SERVICE, "api")], true);
        let svc = build_service(&s, None).unwrap();
        assert_eq!(svc.replica, 1);
    }

    #[test]
    fn a_container_without_a_service_label_is_not_a_service() {
        let s = summary(&[(labels::PROJECT, "demo")], true);
        assert!(build_service(&s, None).is_none());
    }

    #[test]
    fn a_container_without_an_id_is_skipped() {
        let s = summary(&[(labels::SERVICE, "api")], false);
        assert!(build_service(&s, None).is_none());
    }

    /// The one construction three listings share, so their answers stay
    /// comparable. Narrowing to the project is the point of the `Some` case:
    /// without it a second compose project on the same daemon would report
    /// containers this one never streams, and every assembler would look
    /// alive.
    #[test]
    fn project_options_narrow_to_one_project() {
        let options = DockerClient::project_containers(Some("demo"));
        assert!(options.all, "a stopped container has to still be listed");
        assert_eq!(
            options.filters.expect("a label filter")["label"],
            vec![format!("{}=demo", labels::PROJECT)]
        );
    }

    /// `None` matches any value of the label, which is how the project names
    /// themselves are discovered -- a filter naming one project could not.
    #[test]
    fn project_options_without_a_project_match_any_project() {
        let options = DockerClient::project_containers(None);
        assert!(options.all);
        assert_eq!(
            options.filters.expect("a label filter")["label"],
            vec![labels::PROJECT.to_string()]
        );
    }

    /// The key has to name the container the log streams are keyed on, which
    /// is the service plus the replica. The service alone would collapse a
    /// scaled service's containers into one.
    #[test]
    fn container_key_reads_the_service_and_the_replica() {
        assert_eq!(
            container_key(&compose_summary()),
            Some(("api".to_string(), 2))
        );
    }

    /// The same default `build_service` and `LogSupervisor::resync` apply to
    /// the same label, and the three have to agree: a container this called
    /// replica 0 while its log stream called it 1 would look departed for the
    /// whole run.
    #[test]
    fn a_key_without_a_container_number_is_the_first_replica() {
        let s = summary(&[(labels::SERVICE, "api")], true);
        assert_eq!(container_key(&s), Some(("api".to_string(), 1)));
    }

    /// One-off and hook containers are filtered out of the listing the log
    /// streams are planned from, so they have to be filtered out of this one
    /// too. Filtering in only one of the two is how the sets drift apart.
    #[test]
    fn a_transient_container_has_no_key() {
        let oneoff = summary(&[(labels::SERVICE, "api"), (labels::ONEOFF, "True")], true);
        assert!(container_key(&oneoff).is_none());
        let hook = summary(&[(labels::SERVICE, "api"), (labels::HOOK, "start")], true);
        assert!(container_key(&hook).is_none());
    }

    /// The identity is read once, in `container_key`, so that the sidebar and
    /// the fallback's reclaim cannot name the same container differently.
    /// This pins the one thing that reuse shows through: a transient
    /// container has no key, so it can no longer become a `Service` even
    /// where a caller forgot to filter it out first.
    #[test]
    fn a_transient_container_is_not_a_service() {
        let oneoff = summary(&[(labels::SERVICE, "api"), (labels::ONEOFF, "True")], true);
        assert!(build_service(&oneoff, None).is_none());
        let hook = summary(&[(labels::SERVICE, "api"), (labels::HOOK, "start")], true);
        assert!(build_service(&hook, None).is_none());
    }

    /// A container with no service label is not part of the project's graph,
    /// so a key for it could never match anything the streams deliver.
    #[test]
    fn a_container_without_a_service_label_has_no_key() {
        let s = summary(&[(labels::PROJECT, "demo")], true);
        assert!(container_key(&s).is_none());
    }

    /// The same rejection `resync` and `build_service` make. An entry with no
    /// id is one neither of them will attach to, so reporting it alive would
    /// hold an assembler open against a container that never speaks.
    #[test]
    fn a_container_without_an_id_has_no_key() {
        let s = summary(&[(labels::SERVICE, "api")], false);
        assert!(container_key(&s).is_none());
    }

    #[test]
    fn a_running_container_reports_no_exit_code_even_if_docker_remembers_one() {
        // Docker keeps the previous run's ExitCode/FinishedAt after a restart;
        // surfacing them would show a live service as having exited.
        let svc = build_service(
            &compose_summary(),
            Some(&state(ContainerStateStatusEnum::RUNNING, Some(0))),
        )
        .unwrap();
        assert_eq!(svc.status, ServiceStatus::Running);
        assert_eq!(svc.exit_code, None);
        assert_eq!(svc.finished_at, None);
    }

    #[test]
    fn a_finished_container_keeps_its_exit_code_and_finish_time() {
        let svc = build_service(
            &compose_summary(),
            Some(&state(ContainerStateStatusEnum::EXITED, Some(137))),
        )
        .unwrap();
        assert_eq!(svc.status, ServiceStatus::Failure);
        assert_eq!(svc.exit_code, Some(137));
        assert!(svc.finished_at.is_some());
        assert!(svc.duration().is_some());
    }

    #[test]
    fn transient_containers_are_excluded() {
        let oneoff = summary(&[(labels::SERVICE, "api"), (labels::ONEOFF, "True")], true);
        assert!(is_transient(&oneoff));
        let hook = summary(
            &[(labels::SERVICE, "api"), (labels::HOOK, "pre_start")],
            true,
        );
        assert!(is_transient(&hook));
        assert!(!is_transient(&compose_summary()));
        assert!(!is_transient(&ContainerSummary::default()));
    }

    #[test]
    fn a_oneoff_label_that_is_not_true_is_not_transient() {
        let s = summary(&[(labels::SERVICE, "api"), (labels::ONEOFF, "False")], true);
        assert!(!is_transient(&s));
    }

    #[test]
    fn exited_zero_is_success_nonzero_is_failure() {
        let s = |c| derive_status(Some(ContainerStateStatusEnum::EXITED), c, Health::None);
        assert_eq!(s(Some(0)), ServiceStatus::Success);
        assert_eq!(s(Some(1)), ServiceStatus::Failure);
        assert_eq!(s(None), ServiceStatus::Failure);
    }

    #[test]
    fn unhealthy_running_container_is_flagged() {
        let running = Some(ContainerStateStatusEnum::RUNNING);
        assert_eq!(
            derive_status(running, None, Health::Unhealthy),
            ServiceStatus::Unhealthy
        );
        assert_eq!(
            derive_status(running, None, Health::Healthy),
            ServiceStatus::Running
        );
        assert_eq!(
            derive_status(running, None, Health::Starting),
            ServiceStatus::Running
        );
    }

    #[test]
    fn zero_timestamps_are_treated_as_absent() {
        assert!(parse_ts("0001-01-01T00:00:00Z").is_none());
        assert!(parse_ts("2026-09-03T03:12:00.855909554Z").is_some());
    }
}
