//! Lemma 0.21.0 retrocompatibility layer.
//!
//! Holds the compatibility machinery needed to stay source-compatible with the
//! pinned upstream: intermediate wire DTOs, response shaping, behavior
//! adapters, and DB import from a Lemma store.

pub mod wire;
