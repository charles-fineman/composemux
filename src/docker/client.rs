#![allow(clippy::missing_docs_in_private_items)] // 5 left to document
//! Docker Engine API access, scoped to a single compose project.

use crate::docker::labels;
use crate::model::{Health, Service, ServiceStatus};
use anyhow::{Context, Result};
use bollard::models::{
    ContainerState, ContainerStateStatusEnum, ContainerSummary, HealthStatusEnum,
};
use bollard::query_parameters::{InspectContainerOptions, ListContainersOptionsBuilder};
use bollard::Docker;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use std::collections::HashMap;

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

    /// Every container belonging to `project`, one `Service` per container.
    ///
    /// One-off (`compose run`) and lifecycle-hook containers are excluded: they
    /// are transient and would otherwise churn the sidebar.
    pub async fn list_services(&self, project: &str) -> Result<Vec<Service>> {
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("{}={}", labels::PROJECT, project)],
        );
        let options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();

        let summaries = self
            .docker
            .list_containers(Some(options))
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

    /// Distinct compose project names visible to the daemon. Used to give a
    /// useful error when the requested project isn't running.
    pub async fn list_projects(&self) -> Result<Vec<String>> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![labels::PROJECT.to_string()]);
        let options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();

        let summaries = self.docker.list_containers(Some(options)).await?;
        let mut names: Vec<String> = summaries
            .iter()
            .filter_map(|s| s.labels.as_ref()?.get(labels::PROJECT).cloned())
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }
}

/// Builds a `Service` from a list entry and the container's inspected state.
///
/// Kept free of I/O so the label and status mapping can be tested directly.
fn build_service(summary: &ContainerSummary, state: Option<&ContainerState>) -> Option<Service> {
    let labels_map = summary.labels.as_ref()?;
    let name = labels_map.get(labels::SERVICE)?.clone();
    // An unscaled service has no container-number label on some compose
    // versions; treat it as the first (and only) replica.
    let replica = labels_map
        .get(labels::CONTAINER_NUMBER)
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    summary.id.as_ref()?;

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
pub fn is_transient_labels(labels_map: &HashMap<String, String>) -> bool {
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
