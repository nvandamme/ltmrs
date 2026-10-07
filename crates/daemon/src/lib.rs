//! Daemon: lock/lifecycle, local IPC, client registry, scheduling.

pub mod cancellation;
pub mod client;
pub mod dispatcher;
pub mod envelope;
pub mod health;
pub mod idle;
pub mod limits;
pub mod registry;
pub mod runtime;
pub mod scheduler;
pub mod server;
pub mod tools;
