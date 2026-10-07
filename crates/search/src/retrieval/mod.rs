//! Retrieval: filters, parent collapse, RRF, graph, MMR, context.
//!
//! WP-07: Retrieval, graph context and explanations.
//!
//! This module implements the complete retrieval pipeline:
//! - Direct-ID and empty-query routing
//! - Scope resolution and enforcement
//! - Lexical and dense candidate queries
//! - Chunk collapse and rank fusion
//! - Bounded graph expansion
//! - Supersession/conflict bundle resolution
//! - Bundle-aware diversification (MMR)
//! - Context budgeting and explanation

pub mod bundles;
pub mod context;
pub mod engine;
pub mod explain;
pub mod graph_expansion;
pub mod mmr;
pub mod ranking;
pub mod scope;
