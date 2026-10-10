//! The projection worker loop (WP-05 tasks 5, 6; design §8.2).
//!
//! Reads durable desired-state jobs from the canonical repository, renders and
//! embeds only what changed, publishes idempotently to Lance under a per-entity
//! publication guard, then compare-and-clears the job it actually published.
//! Events are retryable wakeups — never an ordered commit log (RV-07).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::search::table::SearchTable;
use ltmrs_domain::id::{EntityId, ModelFingerprint, StoreGeneration};

#[cfg(test)]
mod chunks_tests;
#[cfg(test)]
mod drives_tests;
#[cfg(test)]
mod publish_tests;
#[cfg(test)]
mod test_support;
mod worker;

/// A synchronous embedding provider. The projector never blocks Tokio I/O on it;
/// callers run this off-thread (WP-04's scheduler owns that). Failure leaves the
/// job pending so a stalled embedder retries instead of losing work or blocking
/// lexical indexing.
pub trait Embedder: Send {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String>;

    fn embed_texts(&mut self, texts: &[String]) -> Vec<Result<Vec<f32>, String>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Split rendered memory text into embeddable units (RQ-10). The default is
    /// a single unit covering the whole rendered text — byte-identical to the
    /// pre-chunking behavior. A model-aware override returns one unit per
    /// derived chunk so tail content beyond the first model window gets its
    /// own vector.
    ///
    /// Mapping contract for a future override over the E5 `chunk_passage`
    /// recipe (which yields unprefixed fragment spans with fragment-relative
    /// offsets): re-prefix each span for lexical searchability
    /// (`format!("{title}\n{span}")`) and shift its offsets by
    /// `title.len() + 1` into rendered coordinates; the model embed input
    /// additionally carries the recipe prefix (`passage: {title}\n{span}`).
    /// Precondition: changing the chunking policy bumps the chunker version
    /// (see `chunker_version`), never the model fingerprint — the fingerprint
    /// stays model-bound while the version attributes each row to the policy
    /// that produced it.
    fn chunk_text(&self, title: &str, fragment: &str) -> Vec<TextChunk> {
        vec![single_chunk_unit(title, fragment)]
    }

    /// Chunking-policy version stamped on every projected row. Bump whenever
    /// the chunking policy changes so rows stay attributable per policy.
    fn chunker_version(&self) -> String {
        SINGLE_CHUNK_VERSION.to_string()
    }
}

/// Version stamped by the default single-unit chunking policy.
pub const SINGLE_CHUNK_VERSION: &str = "single-chunk-v1";

/// The default single chunking unit: the whole rendered text. Shared by the
/// `Embedder` default and degraded-mode fallbacks so a memory is never
/// silently dropped when chunking is unavailable.
pub fn single_chunk_unit(title: &str, fragment: &str) -> TextChunk {
    let text = render_text(title, fragment);
    let len = text.len() as u64;
    TextChunk {
        text,
        char_start: 0,
        char_end: len,
    }
}

/// One embeddable unit of a memory: the row's lexical text plus the evidence
/// span of its matched content in rendered-text coordinates
/// (`render_text(title, fragment)` byte offsets).
#[derive(Debug, Clone, PartialEq)]
pub struct TextChunk {
    pub text: String,
    pub char_start: u64,
    pub char_end: u64,
}

/// Deterministic test embedder: a fixed-dimension vector derived from the text.
pub struct FixedEmbedder {
    pub dim: usize,
}

impl Embedder for FixedEmbedder {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        Ok(hash_embed_vec(text, self.dim))
    }
}

/// Deterministic test-vector derivation shared by FixedEmbedder and the
/// test-only hash query embedders (engine tests, quality harness):
/// query/passage spaces must align, so the formula lives in exactly one
/// place. Non-negative by construction (cosine against a negated query is
/// strictly negative — used by threshold tests).
pub fn hash_embed_vec(text: &str, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|i| {
            ((text.bytes().fold(0u64, |a, b| a.wrapping_add(b as u64)) >> (i % 64)) ^ i as u64)
                as f32
                / 1e9
        })
        .collect()
}

/// An embedder that always fails: models a stalled inference worker.
pub struct StalledEmbedder;

impl Embedder for StalledEmbedder {
    fn embed(&mut self, _text: &str) -> Result<Vec<f32>, String> {
        Err("embedding service unavailable".into())
    }
}

/// Outcome of processing one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectorOutcome {
    /// Published (lexical + semantic when the embedder cooperates) and acknowledged.
    Published,
    /// Canonical revision moved on since the job was captured; work left pending.
    StaleRevision,
    /// The memory is no longer recallable (deleted/invalidated); rows removed.
    Tombstoned,
    /// Embedding failed this pass: lexical row published, vector retry stays pending.
    SemanticPending,
}

/// Per-entity publication guard (design §8.2 step 4): serializes the
/// validate-then-publish path per memory so a late old embedding cannot regress
/// a newer row under concurrency within the daemon process (WP-04 guarantees one
/// daemon per store; cross-process ordering is covered by the singleton lock).
#[derive(Default)]
struct PublicationGuards {
    locks: HashMap<EntityId, Arc<Mutex<()>>>,
}

pub struct Projector {
    repo: Arc<ltmrs_service::repository::CanonicalRepository>,
    table: SearchTable,
    embedder: Box<dyn Embedder>,
    fingerprint: ModelFingerprint,
    generation: StoreGeneration,
    guards: PublicationGuards,
    /// Lexical-only mode (no embedding model): text rows publish with NULL
    /// vectors and jobs resolve instead of retrying vector work forever.
    /// Dense rows stay absent until an E5 backfill requeues them.
    lexical_only: bool,
}

impl Projector {
    pub fn new(
        repo: Arc<ltmrs_service::repository::CanonicalRepository>,
        table: SearchTable,
        embedder: Box<dyn Embedder>,
        fingerprint: ModelFingerprint,
        generation: StoreGeneration,
    ) -> Self {
        Self {
            repo,
            table,
            embedder,
            fingerprint,
            generation,
            guards: PublicationGuards::default(),
            lexical_only: false,
        }
    }
}

/// Rendered searchable text: title prefix + fragment body.
pub fn render_text(title: &str, fragment: &str) -> String {
    format!("{title}\n{fragment}")
}
