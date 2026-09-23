//! Upstream pure-function reference oracles (Lemma 0.21.0).
//!
//! These are faithful Rust ports of upstream functions that ltmrs does NOT use
//! as its native behavior (it uses its own calibrated scorer per design §10.3).
//! They are retained as reference oracles for differential testing, mirroring
//! `retrieval::ranking::legacy_reference_score`. Do not wire these into the
//! native ranking/quality path.

/// Milliseconds per day.
const MS_PER_DAY: f64 = 86_400_000.0;

fn clamp01(n: f64) -> f64 {
    n.clamp(0.0, 1.0)
}

/// The counters upstream `calculateQualityScore` reads from a fragment.
#[derive(Debug, Clone, Copy, Default)]
pub struct QualityCounters {
    pub confidence: f64,
    pub positive_feedback: u64,
    pub negative_feedback: u64,
    pub accessed: u64,
    pub refinement_count: u64,
    pub last_accessed_millis: Option<u64>,
    pub negative_hits: u64,
}

/// Upstream `calculateQualityScore` (scoring.ts).
///
/// Composite quality in [0,1] derived from a fragment's own counters.
/// `now_millis` is the clock reference for staleness (deterministic in tests).
pub fn calculate_quality_score(c: &QualityCounters, now_millis: u64) -> f64 {
    let confidence = if c.confidence.is_finite() {
        c.confidence
    } else {
        0.5
    };

    let pos = c.positive_feedback as f64;
    let neg = c.negative_feedback as f64;
    let feedback_total = pos + neg;
    let feedback_ratio = if feedback_total > 0.0 {
        pos / feedback_total
    } else {
        0.5
    };

    let usage_score = ((c.accessed as f64) / 10.0).min(1.0);
    let refinement_score = ((c.refinement_count as f64) / 3.0).min(1.0);

    let days_stale = c
        .last_accessed_millis
        .map_or(0.0, |la| (now_millis as f64 - la as f64) / MS_PER_DAY);
    let staleness_score = clamp01(days_stale / 180.0);

    let neg_hit_penalty = ((c.negative_hits as f64) / 4.0).min(1.0);

    let score = 0.40 * confidence
        + 0.20 * feedback_ratio
        + 0.15 * usage_score
        + 0.10 * refinement_score
        + 0.15 * (1.0 - staleness_score)
        - 0.35 * neg_hit_penalty;

    (clamp01(score) * 1000.0).round() / 1000.0
}

/// Upstream `injectionScore` (core.ts).
///
/// Blended recall priority: confidence (how trusted) × recency (how fresh).
/// `now_millis` is the clock reference (deterministic in tests).
pub fn injection_score(confidence: f64, created_millis: u64, now_millis: u64) -> f64 {
    let days_since_created = (now_millis as f64 - created_millis as f64) / MS_PER_DAY;
    let recency = (1.0 - days_since_created / 180.0).max(0.0);
    confidence * 0.7 + recency * 0.3
}
