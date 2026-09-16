//! The ltmrs domain layer: validated IDs, canonical records, commands,
//! invariants, and the sequential reference interpreter.
//!
//! No database or Arrow types leak into this module.
//! The interpreter is an oracle for concurrency histories, not a production
//! backend candidate.

pub mod clock;
pub mod command;
pub mod export;
pub mod graph;
pub mod guide;
pub mod id;
pub mod interpreter;
pub mod legacy;
pub mod memory;
pub mod project;
pub mod relation;
pub mod session;
