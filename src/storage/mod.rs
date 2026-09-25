//! Storage backend layer.
//!
//! Keeps LanceDB/Arrow types behind this module; exposes domain types to the
//! rest of ltmrs.

pub mod fjall_backend;
// AD-01 (Option B): the Lance-only canonical probe is disqualified as a
// canonical backend (no atomic multi-record path, no uniqueness constraint).
// Retained test-gated so its counterexample regression tests still run under
// `cargo test` without shipping the losing path in production builds.
// Lance remains a production dependency as the WP-05 search projection store
// (see src/search/table.rs, which does not go through this module).
#[cfg(test)]
pub mod lance_backend;
pub mod schema;
