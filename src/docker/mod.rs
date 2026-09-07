pub mod client;
pub mod connection;
pub mod labels;
pub mod stream;

pub use client::{DockerClient, ServiceSource};
pub use connection::{ConnectionHealth, Outage};
pub(crate) use stream::log_debug;
pub use stream::{LogSupervisor, SourceEvent};
