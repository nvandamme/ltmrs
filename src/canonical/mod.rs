//! Canonical repository: the hardened command gateway over the selected
//! backend (Fjall canonical state, per AD-01).
//!
//! Centralizes command application, precondition validation, atomic receipt
//! storage, idempotency, uniqueness and supersession-cycle enforcement.

pub mod repository;

mod repository_internal;

pub use repository::CanonicalRepository;
