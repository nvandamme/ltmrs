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
pub mod cache;
pub mod context;
pub mod engine;
pub mod explain;
pub mod graph_expansion;
pub mod mmr;
pub mod ranking;
pub mod scope;
pub mod tfidf;

/// Outcome of the live-snapshot expansion probe: reachability under the
/// default graph policy over real staged edges, vs the neutered
/// (relation-agnostic) baseline.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotLift {
    /// Hub seeds probed.
    pub seeds: usize,
    /// Mean nodes reached per seed under the default policy (agnostic
    /// baseline always reaches exactly 1.0: the seed itself).
    pub mean_reached: f64,
    /// Fraction of seeds whose expansion hit a policy bound.
    pub truncation_rate: f64,
    /// Per-type edge counts in the snapshot.
    pub by_type: std::collections::BTreeMap<String, usize>,
}

/// Load a `graph-probe.json` edge export (see `tmp/live-lemma`, gitignored
/// scratch) and run the real [`graph_expansion::expand`] from the 20
/// highest-degree nodes under the default policy. Test support for the
/// graph ablation leg: real staged edges, no synthesis. Reports
/// reachability lift, never recall.
#[cfg(test)]
pub fn snapshot_expansion_lift(path: &std::path::Path) -> Result<SnapshotLift, String> {
    use graph_expansion::GraphPolicy;
    use ltmrs_domain::id::EntityId;
    use ltmrs_domain::memory::Instant;
    use ltmrs_domain::relation::{Relation, RelationType};

    #[derive(serde::Deserialize)]
    struct Edge {
        s: String,
        t: String,
        ty: String,
    }
    #[derive(serde::Deserialize)]
    struct Probe {
        edges: Vec<Edge>,
    }
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let probe: Probe = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let parse_id = |s: &str| {
        uuid::Uuid::parse_str(s)
            .map(EntityId::new)
            .map_err(|e| e.to_string())
    };
    let parse_ty = |ty: &str| match ty {
        "supports" => Ok(RelationType::Supports),
        "contradicts" => Ok(RelationType::Contradicts),
        "supersedes" => Ok(RelationType::Supersedes),
        "superseded_by" => Ok(RelationType::SupersededBy),
        "related_to" => Ok(RelationType::RelatedTo),
        other => Err(format!("unknown relation type {other:?}")),
    };
    let mut neighbors: std::collections::BTreeMap<EntityId, Vec<Relation>> =
        std::collections::BTreeMap::new();
    let mut degree: std::collections::BTreeMap<EntityId, usize> = std::collections::BTreeMap::new();
    let mut by_type: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (i, e) in probe.edges.iter().enumerate() {
        let (s, t) = (parse_id(&e.s)?, parse_id(&e.t)?);
        let ty = parse_ty(&e.ty)?;
        *by_type.entry(e.ty.clone()).or_default() += 1;
        *degree.entry(s).or_default() += 1;
        *degree.entry(t).or_default() += 1;
        neighbors.entry(s).or_default().push(Relation::new(
            EntityId::new(uuid::Uuid::from_u128(i as u128)),
            s,
            t,
            ty,
            None,
            Instant::new(1000),
        ));
    }
    // Seeds: 20 highest-degree nodes (hub queries — the expansion path
    // the relation-agnostic baseline cannot take).
    let mut hubs: Vec<(EntityId, usize)> = degree.into_iter().collect();
    hubs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    hubs.truncate(20);
    let policy = GraphPolicy::default();
    let mut total_reached = 0usize;
    let mut truncated = 0usize;
    for (seed, _) in &hubs {
        let exp = graph_expansion::expand(
            std::slice::from_ref(seed),
            &|id| neighbors.get(&id).cloned().unwrap_or_default(),
            &|_| true,
            &policy,
        );
        total_reached += exp.nodes.len();
        truncated += usize::from(exp.truncated);
    }
    Ok(SnapshotLift {
        seeds: hubs.len(),
        mean_reached: total_reached as f64 / hubs.len().max(1) as f64,
        truncation_rate: truncated as f64 / hubs.len().max(1) as f64,
        by_type,
    })
}
