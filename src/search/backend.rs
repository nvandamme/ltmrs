//! Search backend for the daemon (WP-08): wraps the Lance search table and a
//! query embedder so the dispatcher can run lexical + dense retrieval.
//!
//! The dispatcher is synchronous (it runs on `spawn_blocking`), so async
//! search is bridged with `Handle::current().block_on()`. The embedder is a
//! seam: production wires the Candle E5-small service; tests use a
//! deterministic adapter.

use std::sync::{Arc, Mutex};

use crate::domain::command::{DomainError, DomainErrorCode, DomainResult};
use crate::embeddings::e5_small::{Chunk, E5SmallAdapter, EmbedInput};
use crate::embeddings::recipe::Role;
use crate::retrieval::engine::{Engine, QueryEmbedder, RetrievalRequest, RetrievalResult};
use crate::search::projector::{Embedder, TextChunk};
use crate::search::table::SearchTable;
use crate::service::repository::CanonicalRepository;

/// The query embedder seam, shared across searches.
pub trait QueryEmbedderProvider: Send + Sync {
    /// Embed a query text into a vector.
    fn embed_query(&self, query: &str) -> DomainResult<Vec<f32>>;
}

/// A search backend: the canonical repository + Lance table + embedder.
///
/// The dense leg's model fingerprint travels per-request in
/// `RetrievalRequest::model_fingerprint`, so the backend caches no
/// fingerprint/generation state (it may hold model weights behind the
/// embedder trait object).
pub struct SearchBackend {
    repo: Arc<CanonicalRepository>,
    table: SearchTable,
    embedder: Arc<dyn QueryEmbedderProvider>,
}

impl SearchBackend {
    pub fn new(
        repo: Arc<CanonicalRepository>,
        table: SearchTable,
        embedder: Arc<dyn QueryEmbedderProvider>,
    ) -> Self {
        Self {
            repo,
            table,
            embedder,
        }
    }

    /// Run a retrieval request synchronously (bridges async via block_on).
    ///
    /// Must be called from a Tokio runtime context (the dispatcher runs on
    /// `spawn_blocking`, which inherits the runtime handle).
    pub fn retrieve_sync(&self, req: &RetrievalRequest) -> DomainResult<RetrievalResult> {
        let handle = tokio::runtime::Handle::try_current().map_err(|e| {
            crate::domain::command::DomainError::new(
                crate::domain::command::DomainErrorCode::Validation,
                format!("no tokio runtime for search: {e}"),
            )
        })?;
        let engine = Engine::new(self.repo.clone(), self.table.clone());
        let embedder = Arc::clone(&self.embedder);
        handle.block_on(async move {
            engine
                .retrieve(req, &QueryEmbedderAdapter { embedder })
                .await
        })
    }

    /// Whether the FTS index is ready (for readiness reporting).
    pub fn fts_ready(&self) -> DomainResult<bool> {
        let handle = tokio::runtime::Handle::try_current().map_err(|e| {
            crate::domain::command::DomainError::new(
                crate::domain::command::DomainErrorCode::Validation,
                format!("no tokio runtime for search: {e}"),
            )
        })?;
        let table = self.table.clone();
        Ok(handle.block_on(async move { table.fts_index_ready().await.unwrap_or(false) }))
    }
}

/// Adapts a `QueryEmbedderProvider` to the engine's `QueryEmbedder` trait.
struct QueryEmbedderAdapter {
    embedder: Arc<dyn QueryEmbedderProvider>,
}

impl QueryEmbedder for QueryEmbedderAdapter {
    fn embed_query<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>>
    {
        let embedder = Arc::clone(&self.embedder);
        let query = query.to_string();
        Box::pin(async move { embedder.embed_query(&query) })
    }
}

type EmbedFn = Box<dyn Fn(&str) -> DomainResult<Vec<f32>> + Send + Sync>;

/// A simple synchronous query embedder backed by a closure (for tests and the
/// fixed-dimension adapter).
pub struct ClosureEmbedder {
    f: EmbedFn,
}

impl ClosureEmbedder {
    pub fn new(f: impl Fn(&str) -> DomainResult<Vec<f32>> + Send + Sync + 'static) -> Self {
        Self { f: Box::new(f) }
    }
}

impl QueryEmbedderProvider for ClosureEmbedder {
    fn embed_query(&self, query: &str) -> DomainResult<Vec<f32>> {
        (self.f)(query)
    }
}

/// A Mutex-guarded synchronous embedder (for the Candle adapter, which is
/// `&mut self`).
pub struct MutexEmbedder {
    inner: Mutex<Box<dyn crate::search::projector::Embedder + Send>>,
}

impl MutexEmbedder {
    pub fn new(inner: Box<dyn crate::search::projector::Embedder + Send>) -> Self {
        Self {
            inner: Mutex::new(inner),
        }
    }
}

impl QueryEmbedderProvider for MutexEmbedder {
    fn embed_query(&self, query: &str) -> DomainResult<Vec<f32>> {
        let mut guard = self.inner.lock().map_err(|_| {
            crate::domain::command::DomainError::new(
                crate::domain::command::DomainErrorCode::Validation,
                "embedder lock poisoned",
            )
        })?;
        guard.embed(query).map_err(|e| {
            crate::domain::command::DomainError::new(
                crate::domain::command::DomainErrorCode::Validation,
                format!("embedding failed: {e}"),
            )
        })
    }
}

/// Chunking-policy version for rows built through the E5 mapping below.
/// Bumped whenever the mapping changes; the model fingerprint stays
/// model-bound (see `SearchRow::chunker_version`).
pub const E5_CHUNK_VERSION: &str = "e5-chunks-v1";

/// Map E5 derived chunks (verbatim fragment spans, fragment-relative offsets)
/// to projector units: re-prefix each span for lexical searchability and
/// shift its offsets by the rendered title prefix into rendered coordinates
/// (the contract `Embedder::chunk_text` documents).
pub fn e5_chunks_to_text_chunks(title: &str, chunks: &[Chunk]) -> Vec<TextChunk> {
    let base = title.len() as u64 + 1; // "title\n" rendered prefix
    chunks
        .iter()
        .map(|c| TextChunk {
            text: format!("{title}\n{}", c.text),
            char_start: base + c.char_start as u64,
            char_end: base + c.char_end as u64,
        })
        .collect()
}

fn poisoned_lock() -> DomainError {
    DomainError::new(DomainErrorCode::Validation, "embedder lock poisoned")
}

/// Document-side bridge: the projector's `Embedder` seam over the pinned
/// E5-small adapter (Passage role). Oversized input errors instead of
/// truncating; the projector treats that as pending semantic work.
impl Embedder for E5SmallAdapter {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        self.embed_batch(&[EmbedInput {
            text: text.to_string(),
            role: Role::Passage,
        }])
        .map_err(|e| e.to_string())?
        .pop()
        .map(|s| s.vector)
        .ok_or_else(|| "embedding returned no sequences".to_string())
    }

    fn chunk_text(&self, title: &str, fragment: &str) -> Vec<TextChunk> {
        e5_chunks_to_text_chunks(title, &self.chunk_passage(title, fragment))
    }

    fn chunker_version(&self) -> String {
        E5_CHUNK_VERSION.to_string()
    }
}

/// Query-side bridge: E5 with the Query role. Prefix asymmetry is
/// load-bearing for quality — queries must never go through the
/// Passage-role document seam above; the type separation enforces that.
///
/// Runs inference inline behind a mutex (the dispatcher already runs on
/// `spawn_blocking`, so Tokio core workers are not blocked). Routing queries
/// through the bounded cancellable worker (`EmbeddingService`) is deferred —
/// this path is unbounded, serialized and non-cancellable (RQ-22 follow-up).
impl QueryEmbedderProvider for Mutex<E5SmallAdapter> {
    fn embed_query(&self, query: &str) -> DomainResult<Vec<f32>> {
        let mut guard = self.lock().map_err(|_| poisoned_lock())?;
        guard
            .embed_batch(&[EmbedInput {
                text: query.to_string(),
                role: Role::Query,
            }])
            .map_err(|e| {
                DomainError::new(
                    DomainErrorCode::Validation,
                    format!("query embedding failed: {e}"),
                )
            })?
            .pop()
            .map(|s| s.vector)
            .ok_or_else(|| {
                DomainError::new(
                    DomainErrorCode::Validation,
                    "query embedding returned no sequences",
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::e5_small::Chunk;

    /// Policy versions are distinct strings: the E5 recipe must never share
    /// the default single-chunk version.
    #[test]
    fn e5_chunk_version_differs_from_default() {
        use crate::search::projector::SINGLE_CHUNK_VERSION;
        assert!(!E5_CHUNK_VERSION.is_empty());
        assert_ne!(E5_CHUNK_VERSION, SINGLE_CHUNK_VERSION);
    }

    /// The E5 chunk mapping re-prefixes each verbatim fragment span for
    /// lexical searchability and shifts its offsets into rendered coordinates.
    #[test]
    fn e5_chunk_mapping_reprefixes_and_shifts_to_rendered_coords() {
        let chunks = vec![
            Chunk {
                text: "alpha".into(),
                char_start: 0,
                char_end: 5,
                token_count: 3,
            },
            Chunk {
                text: "beta".into(),
                char_start: 6,
                char_end: 10,
                token_count: 2,
            },
        ];
        let units = e5_chunks_to_text_chunks("T", &chunks);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].text, "T\nalpha");
        assert_eq!((units[0].char_start, units[0].char_end), (2, 7));
        assert_eq!(units[1].text, "T\nbeta");
        assert_eq!((units[1].char_start, units[1].char_end), (8, 12));
    }

    /// An empty chunk set maps to no units (the projector's own fallback
    /// covers a misbehaving embedder; the mapping itself adds nothing).
    #[test]
    fn e5_chunk_mapping_preserves_empty() {
        assert!(e5_chunks_to_text_chunks("T", &[]).is_empty());
    }
}
