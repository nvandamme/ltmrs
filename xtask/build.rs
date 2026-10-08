//! Linker auto-detection shim: shared probe lives in
//! `tools/detect_linker.rs` (one implementation for all members,
//! since `cargo::rustc-link-arg` does not propagate to dependents).
include!("../tools/detect_linker.rs");
