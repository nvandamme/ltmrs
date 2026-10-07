//! Embedding subsystem (WP-06): pinned model manifest, verified artifact cache,
//! the qualified E5-small Candle adapter, tokenizer-aware chunking and a bounded
//! synchronous inference worker exposed through an async service.
//!
//! Design refs: plans/01_design_and_concepts.md §9; WP-06 in 02_implementation_guide.

pub mod artifacts;
pub mod bert_impl;
pub mod e5_small;
pub mod manifest;
pub mod recipe;
pub mod service;
pub mod worker;
