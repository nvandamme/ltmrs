//! Storage backend layer.
//!
//! Keeps LanceDB/Arrow types behind this module; exposes domain types to the
//! rest of ltmrs.

pub mod fjall_backend;
pub mod lance_backend;
pub mod schema;
