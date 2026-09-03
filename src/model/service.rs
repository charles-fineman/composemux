//! The domain model: a compose service and its runtime status.
//!
//! `ServiceStatus` mirrors nx's `TaskStatus` so that the status→colour and
//! status→icon mappings port across unchanged.

use chrono::{DateTime, Utc};

/// Compose analogue of nx's `TaskStatus`.
///
/// | nx            | here         | container condition              |
/// |---------------|--------------|----------------------------------|
/// | `InProgress`  | `Running`    | running                          |
/// | `Success`     | `Success`    | exited 0                         |
/// | `Failure`     | `Failure`    | exited non-zero / dead / OOM     |
/// | `Skipped`     | `Unhealthy`  | health unhealthy, or restarting  |
/// | `Stopped`     | `Stopped`    | paused / stopping / removing     |
/// | `NotStarted`  | `NotStarted` | declared but no container yet    |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    Running,
    Success,
    Failure,
    Unhealthy,
    Stopped,
    NotStarted,
}

impl ServiceStatus {
    /// Whether the service has reached a terminal state. Used to decide when the
    /// whole stack is down and the auto-exit countdown should start.
    pub fn is_finished(self) -> bool {
        matches!(self, ServiceStatus::Success | ServiceStatus::Failure)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// No healthcheck declared.
    None,
    Starting,
    Healthy,
    Unhealthy,
}

impl Health {
    /// Text for the health column. `"-"` matches nx's not-applicable cache cell.
    pub fn label(self) -> &'static str {
        match self {
            Health::None => "-",
            Health::Starting => "start",
            Health::Healthy => "ok",
            Health::Unhealthy => "fail",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Service {
    /// The compose service name (`com.docker.compose.service`).
    pub name: String,
    /// Replica index (`com.docker.compose.container-number`), 1-based.
    pub replica: u32,
    pub status: ServiceStatus,
    pub health: Health,
    pub exit_code: Option<i64>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl Service {
    /// How long the service has been up, or how long it ran before exiting.
    pub fn duration(&self) -> Option<chrono::Duration> {
        let start = self.started_at?;
        let end = self.finished_at.unwrap_or_else(Utc::now);
        Some(end - start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0).unwrap()
    }

    fn service(status: ServiceStatus) -> Service {
        Service {
            name: "api".into(),
            replica: 1,
            status,
            health: Health::None,
            exit_code: None,
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn only_exited_services_count_as_finished() {
        assert!(ServiceStatus::Success.is_finished());
        assert!(ServiceStatus::Failure.is_finished());
        for status in [
            ServiceStatus::Running,
            ServiceStatus::Unhealthy,
            ServiceStatus::Stopped,
            ServiceStatus::NotStarted,
        ] {
            assert!(!status.is_finished(), "{status:?} is not a terminal state");
        }
    }

    #[test]
    fn a_finished_service_reports_the_span_it_ran_for() {
        let mut svc = service(ServiceStatus::Success);
        svc.started_at = Some(at(1));
        svc.finished_at = Some(at(4));
        assert_eq!(svc.duration().unwrap().num_hours(), 3);
    }

    #[test]
    fn a_running_service_measures_against_now() {
        let mut svc = service(ServiceStatus::Running);
        svc.started_at = Some(Utc::now() - chrono::Duration::seconds(30));
        let elapsed = svc.duration().expect("a started service has a duration");
        assert!((29..=31).contains(&elapsed.num_seconds()), "got {elapsed}");
    }

    #[test]
    fn a_service_that_never_started_has_no_duration() {
        assert!(service(ServiceStatus::NotStarted).duration().is_none());
    }

    #[test]
    fn health_labels_fit_the_column() {
        for health in [
            Health::None,
            Health::Starting,
            Health::Healthy,
            Health::Unhealthy,
        ] {
            assert!(health.label().len() <= 6, "{health:?} label is too wide");
        }
    }
}
