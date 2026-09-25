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
    embedder: Arc<dyn QueryEmbedder>,
}

impl SearchBackend {
    pub fn new(
        repo: Arc<CanonicalRepository>,
        table: SearchTable,
        embedder: Arc<dyn QueryEmbedder>,
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
    /// `spawn_blocking`, which inherits the runtime handle). The single
    /// outer bridge contains no nested blocking: the engine awaits the
    /// embedder future normally.
    pub fn retrieve_sync(&self, req: &RetrievalRequest) -> DomainResult<RetrievalResult> {
        let handle = tokio::runtime::Handle::try_current().map_err(|e| {
            crate::domain::command::DomainError::new(
                crate::domain::command::DomainErrorCode::Validation,
                format!("no tokio runtime for search: {e}"),
            )
        })?;
        let engine = Engine::new(self.repo.clone(), self.table.clone());
        let embedder = Arc::clone(&self.embedder);
        handle.block_on(async move { engine.retrieve(req, embedder.as_ref()).await })
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

/// Adapts a synchronous `QueryEmbedderProvider` to the engine's async
/// `QueryEmbedder` trait (for tests and fixed-dimension adapters). Never
/// blocks: the provider runs inline in the caller's async context.
pub struct QueryEmbedderAdapter {
    embedder: Arc<dyn QueryEmbedderProvider>,
}

impl QueryEmbedderAdapter {
    pub fn new(embedder: Arc<dyn QueryEmbedderProvider>) -> Self {
        Self { embedder }
    }
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
/// `spawn_blocking`, so Tokio core workers are not blocked). Prefer
/// [`ServiceQueryEmbedder`] where a worker exists: bounded queue,
/// batch accounting and shutdown semantics (RQ-22).
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

/// Query-side bridge over the bounded worker (design §7.3, RQ-22): queries
/// flow through the worker's queue (backpressure instead of unbounded
/// inline inference) with batch accounting and shutdown semantics.
/// Fully async — awaiting it never blocks, and dropping the future cancels
/// the request at the worker. Overload surfaces as a `busy:`-prefixed
/// error (retryable); every other failure is a plain embedding error.
pub struct ServiceQueryEmbedder {
    service: crate::embeddings::service::EmbeddingService,
}

impl ServiceQueryEmbedder {
    pub fn new(service: crate::embeddings::service::EmbeddingService) -> Self {
        Self { service }
    }
}

impl QueryEmbedder for ServiceQueryEmbedder {
    fn embed_query<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>>
    {
        let service = self.service.clone();
        let query = query.to_string();
        Box::pin(async move {
            service.embed(&query, Role::Query).await.map_err(|e| {
                let message = e.to_string();
                // "busy:" marks retryable backpressure; DomainErrorCode has
                // no overload variant, so the prefix is the contract.
                let message = match &e {
                    crate::embeddings::worker::ServiceError::Busy => {
                        format!("busy: {message}")
                    }
                    _ => format!("query embedding failed: {message}"),
                };
                DomainError::new(DomainErrorCode::Validation, message)
            })
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

    /// Service-routed queries embed with the Query role through the bounded
    /// worker (design §7.3), awaiting normally — never blocking the caller.
    #[tokio::test]
    async fn service_backed_query_embeds_with_query_role() {
        use crate::embeddings::artifacts::ArtifactResult;
        use crate::embeddings::e5_small::{EmbedInput, EmbeddedSequence};
        use crate::embeddings::recipe::Role;
        use crate::embeddings::service::EmbeddingService;
        use crate::embeddings::worker::{EmbeddingWorkerConfig, SyncEmbedder};
        use std::sync::Mutex as StdMutex;

        struct RecordingEmbedder {
            roles: std::sync::Arc<StdMutex<Vec<Role>>>,
        }
        impl SyncEmbedder for RecordingEmbedder {
            fn embed_batch(
                &mut self,
                inputs: &[EmbedInput],
            ) -> ArtifactResult<Vec<EmbeddedSequence>> {
                self.roles
                    .lock()
                    .unwrap()
                    .extend(inputs.iter().map(|i| i.role));
                Ok(inputs
                    .iter()
                    .map(|_| EmbeddedSequence {
                        vector: vec![0.25; 384],
                        input_ids: vec![],
                        attention_mask: vec![],
                    })
                    .collect())
            }
        }

        let roles = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let svc = EmbeddingService::spawn(
            RecordingEmbedder {
                roles: std::sync::Arc::clone(&roles),
            },
            EmbeddingWorkerConfig::default(),
        );
        let provider = ServiceQueryEmbedder::new(svc);
        let vec = provider.embed_query("how to cut over").await.unwrap();
        assert_eq!(vec, vec![0.25; 384]);
        assert_eq!(*roles.lock().unwrap(), vec![Role::Query]);
    }

    /// A closed worker surfaces as an error, never a hang or a zero vector.
    #[tokio::test]
    async fn service_backed_query_after_shutdown_is_error() {
        use crate::embeddings::service::EmbeddingService;
        use crate::embeddings::worker::EmbeddingWorkerConfig;

        struct EmptyEmbedder;
        impl crate::embeddings::worker::SyncEmbedder for EmptyEmbedder {
            fn embed_batch(
                &mut self,
                inputs: &[crate::embeddings::e5_small::EmbedInput],
            ) -> crate::embeddings::artifacts::ArtifactResult<
                Vec<crate::embeddings::e5_small::EmbeddedSequence>,
            > {
                Ok(inputs
                    .iter()
                    .map(|_| crate::embeddings::e5_small::EmbeddedSequence {
                        vector: vec![0.0; 384],
                        input_ids: vec![],
                        attention_mask: vec![],
                    })
                    .collect())
            }
        }

        let svc = EmbeddingService::spawn(EmptyEmbedder, EmbeddingWorkerConfig::default());
        svc.shutdown();
        let provider = ServiceQueryEmbedder::new(svc);
        let err = provider.embed_query("q").await.unwrap_err();
        assert!(
            err.message.contains("worker")
                || err.message.contains("Closed")
                || err.message.contains("closed"),
            "closed worker must surface, got: {}",
            err.message
        );
    }

    /// Overload surfaces as a retryable `busy:` error through the bridge,
    /// never silent fallback material at this layer.
    #[tokio::test]
    async fn service_backed_query_busy_is_retryable() {
        use crate::embeddings::service::EmbeddingService;
        use crate::embeddings::worker::{EmbeddingWorkerConfig, SyncEmbedder};
        use crate::retrieval::engine::QueryEmbedder;

        struct SlowEmbedder;
        impl SyncEmbedder for SlowEmbedder {
            fn embed_batch(
                &mut self,
                inputs: &[crate::embeddings::e5_small::EmbedInput],
            ) -> crate::embeddings::artifacts::ArtifactResult<
                Vec<crate::embeddings::e5_small::EmbeddedSequence>,
            > {
                std::thread::sleep(std::time::Duration::from_millis(500));
                Ok(inputs
                    .iter()
                    .map(|_| crate::embeddings::e5_small::EmbeddedSequence {
                        vector: vec![0.0; 384],
                        input_ids: vec![],
                        attention_mask: vec![],
                    })
                    .collect())
            }
        }

        let svc = EmbeddingService::spawn(
            SlowEmbedder,
            EmbeddingWorkerConfig {
                max_batch_size: 32,
                max_queue_depth: 1,
            },
        );
        // Occupy the worker, then fill the single queue slot.
        let w1 = tokio::spawn({
            let svc = svc.clone();
            async move {
                svc.embed("first", crate::embeddings::recipe::Role::Query)
                    .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let w2 = tokio::spawn({
            let svc = svc.clone();
            async move {
                svc.embed("second", crate::embeddings::recipe::Role::Query)
                    .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // Worker busy + queue full: the bridge must report retryable busy.
        let provider = ServiceQueryEmbedder::new(svc);
        let err = provider.embed_query("third").await.unwrap_err();
        assert!(
            err.message.starts_with("busy:"),
            "overload must be retryable busy, got: {}",
            err.message
        );
        let _ = w1.await.unwrap();
        let _ = w2.await.unwrap();
    }

    /// Nested retrieve_sync through the service bridge: the production dense
    /// shape (outer bridge + engine await + service await) with no nested
    /// blocking. Would panic on any nested block_on.
    #[tokio::test]
    async fn retrieve_sync_through_service_bridge_does_not_nest_block() {
        use crate::embeddings::service::EmbeddingService;
        use crate::embeddings::worker::{EmbeddingWorkerConfig, SyncEmbedder};
        use crate::retrieval::engine::RetrievalRequest;

        struct ConstEmbedder;
        impl SyncEmbedder for ConstEmbedder {
            fn embed_batch(
                &mut self,
                inputs: &[crate::embeddings::e5_small::EmbedInput],
            ) -> crate::embeddings::artifacts::ArtifactResult<
                Vec<crate::embeddings::e5_small::EmbeddedSequence>,
            > {
                CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(inputs
                    .iter()
                    .map(|_| crate::embeddings::e5_small::EmbeddedSequence {
                        vector: vec![1.0; 384],
                        input_ids: vec![],
                        attention_mask: vec![],
                    })
                    .collect())
            }
        }
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        let dir = tempfile::tempdir().unwrap();
        let table = crate::search::table::SearchTable::open(dir.path().to_str().unwrap())
            .await
            .unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = std::sync::Arc::new(
            crate::service::repository::CanonicalRepository::open(
                repo_dir.path().to_str().unwrap(),
            )
            .unwrap(),
        );
        let svc = EmbeddingService::spawn(ConstEmbedder, EmbeddingWorkerConfig::default());
        let backend = SearchBackend::new(
            repo,
            table,
            std::sync::Arc::new(ServiceQueryEmbedder::new(svc)),
        );
        let req = RetrievalRequest {
            query: "anything".into(),
            // Dense leg on: the engine must await the service through the
            // outer bridge with no nested blocking anywhere.
            model_fingerprint: Some(crate::domain::id::ModelFingerprint::new(1)),
            ..Default::default()
        };
        // Same sync-context rule as the dispatcher: bridge from blocking code.
        let out = tokio::task::spawn_blocking(move || backend.retrieve_sync(&req))
            .await
            .unwrap()
            .unwrap();
        assert!(out.results.is_empty(), "empty table recalls nothing");
        // The dense leg ran: a future early-exit skipping the embed would
        // keep this green while voiding the nesting proof.
        assert_eq!(
            CALLS.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "dense leg must invoke the worker exactly once"
        );
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
