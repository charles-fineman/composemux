//! Compose v2 container labels.
//!
//! Declared here rather than pulled from `docker/compose` as a dependency:
//! Docker does not guarantee compatibility of compose internals between
//! versions, and these three names are stable public contract.
//! Verified against a live Compose v5.5.0 project.

pub const PROJECT: &str = "com.docker.compose.project";
pub const SERVICE: &str = "com.docker.compose.service";
pub const CONTAINER_NUMBER: &str = "com.docker.compose.container-number";
/// `"True"` on containers created by `docker compose run`.
pub const ONEOFF: &str = "com.docker.compose.oneoff";
/// Set on ephemeral lifecycle-hook containers.
pub const HOOK: &str = "com.docker.compose.hook";
