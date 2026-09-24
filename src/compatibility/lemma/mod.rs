//! Lemma 0.21.0 retrocompatibility layer.
//!
//! Holds the compatibility machinery needed to stay source-compatible with the
//! pinned upstream: frozen wire schemas, typed tool arguments, response
//! shaping, behavior adapters, and DB import from a Lemma store.

pub mod intelligence;
pub mod privacy;
pub mod reference;
pub mod schemas;
pub mod tool_args;
pub mod wire;
