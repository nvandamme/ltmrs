//! Explanation schema mapping (WP-07 task 12).
//!
//! Records, for a single retrieval call, the candidate ranks, applied filters,
//! revision/generation, graph paths, score components, diversification
//! decisions and excluded/truncated context. Each pipeline stage exposes
//! deterministic diagnostics so tests can assert on the reasoning, not just
//! the final list.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::id::{EntityId, ModelFingerprint, StoreGeneration};

/// The version of the retrieval profile this explanation was produced under.
/// Frozen so held-out evaluation (RQ-24) has a stable reference.
pub const RETRIEVAL_PROFILE_VERSION: &str = "1.0.0";

/// Per-leg candidate ranks for one entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegRank {
    pub leg: String,
    pub rank: usize,
}

/// Score components for one candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreComponents {
    /// Normalized RRF R(d) in [0,1].
    pub rrf_normalized: f64,
    /// Bounded graph contribution G(d) in [0,1].
    pub graph: f64,
    /// Clamped priority P(d) in [0,1].
    pub priority: f64,
    /// Final calibrated native score S(d) in [0,1].
    pub native_score: f64,
    /// Legacy reference score (oracle only), for separate evidence.
    pub legacy_reference: f64,
}

/// Explanation for one candidate in the result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateExplanation {
    pub id: EntityId,
    /// Final position in the returned list (one-based).
    pub position: usize,
    /// Ranks in each retrieval leg.
    pub leg_ranks: Vec<LegRank>,
    /// The score components.
    pub scores: ScoreComponents,
    /// Graph path from a seed, if reached via expansion.
    pub graph_path: Option<GraphPathRecord>,
    /// Whether the candidate is protected (bundle member).
    pub protected: bool,
    /// Whether the candidate was dropped by diversification.
    pub excluded_by_diversity: bool,
}

/// A graph path record for the explanation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphPathRecord {
    pub nodes: Vec<EntityId>,
    pub edge_types: Vec<String>,
    pub depth: usize,
}

/// The full explanation for a retrieval call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalExplanation {
    /// The retrieval profile version.
    pub profile_version: String,
    /// The store generation this explanation was produced under.
    pub store_generation: StoreGeneration,
    /// The model fingerprint for the dense leg.
    pub model_fingerprint: Option<ModelFingerprint>,
    /// The effective scope filter (as a human-readable predicate).
    pub scope_filter: Option<String>,
    /// Whether the query was empty (list/priority mode).
    pub empty_query: bool,
    /// Whether the query matched nothing (valid no-answer).
    pub no_match: bool,
    /// Per-candidate explanations, keyed by ID.
    pub candidates: BTreeMap<EntityId, CandidateExplanation>,
    /// IDs excluded due to context budget.
    pub excluded_by_budget: Vec<EntityId>,
    /// A conflict notice, if one was emitted.
    pub conflict_notice: Option<String>,
    /// Whether graph expansion was truncated by a limit.
    pub graph_truncated: bool,
    /// Whether the FTS index is available (lexical leg is ready).
    pub fts_ready: bool,
    /// Whether any projected rows carry vectors (dense leg is ready).
    pub dense_ready: bool,
    /// Pending projection jobs (canonical writes not yet projected).
    pub projection_lag: usize,
    /// Whether this result is PARTIAL: recall may be incomplete because the
    /// projection is not yet converged or an index is not yet built.
    pub partial: bool,
}

impl Default for RetrievalExplanation {
    fn default() -> Self {
        Self {
            profile_version: RETRIEVAL_PROFILE_VERSION.to_string(),
            store_generation: StoreGeneration::FIRST,
            model_fingerprint: None,
            scope_filter: None,
            empty_query: false,
            no_match: false,
            candidates: BTreeMap::new(),
            excluded_by_budget: Vec::new(),
            conflict_notice: None,
            graph_truncated: false,
            fts_ready: false,
            dense_ready: false,
            projection_lag: 0,
            partial: false,
        }
    }
}

impl RetrievalExplanation {
    pub fn new(
        store_generation: StoreGeneration,
        model_fingerprint: Option<ModelFingerprint>,
    ) -> Self {
        Self {
            store_generation,
            model_fingerprint,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::EntityId;
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    #[test]
    fn explanation_serializes_round_trip() {
        let mut exp = RetrievalExplanation::new(StoreGeneration::FIRST, None);
        exp.empty_query = false;
        exp.no_match = false;
        exp.scope_filter = Some("project IS NULL".into());
        exp.candidates.insert(
            eid(1),
            CandidateExplanation {
                id: eid(1),
                position: 1,
                leg_ranks: vec![LegRank {
                    leg: "lexical".into(),
                    rank: 1,
                }],
                scores: ScoreComponents {
                    rrf_normalized: 1.0,
                    graph: 0.0,
                    priority: 0.5,
                    native_score: 0.95,
                    legacy_reference: 0.03,
                },
                graph_path: None,
                protected: false,
                excluded_by_diversity: false,
            },
        );

        let json = serde_json::to_string(&exp).unwrap();
        let back: RetrievalExplanation = serde_json::from_str(&json).unwrap();
        assert_eq!(exp, back);
    }

    #[test]
    fn profile_version_is_frozen() {
        let exp = RetrievalExplanation::new(StoreGeneration::FIRST, None);
        assert_eq!(exp.profile_version, RETRIEVAL_PROFILE_VERSION);
    }

    #[test]
    fn no_match_flag_serializes() {
        let mut exp = RetrievalExplanation::new(StoreGeneration::FIRST, None);
        exp.no_match = true;
        let json = serde_json::to_string(&exp).unwrap();
        assert!(json.contains("\"no_match\":true"));
    }
}
