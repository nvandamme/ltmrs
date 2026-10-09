//! Search backend for the daemon (WP-08): wraps the Lance search table and a
//! query embedder so the dispatcher can run lexical + dense retrieval.
//!
//! The dispatcher is synchronous (it runs on `spawn_blocking`), so async
//! search is bridged with `Handle::current().block_on()`. The embedder is a
//! seam: production wires the Candle E5-small service; tests use a
//! deterministic adapter.

use std::sync::{Arc, Mutex};

use crate::retrieval::engine::{Engine, QueryEmbedder, RetrievalRequest, RetrievalResult};
use crate::search::projector::{Embedder, TextChunk};
use crate::search::table::SearchTable;
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_embeddings::e5_small::{Chunk, E5SmallAdapter, EmbedInput};
use ltmrs_embeddings::recipe::Role;
use ltmrs_service::repository::CanonicalRepository;

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
/// embedder trait object). The backend-level fingerprint records whether
/// THIS backend can serve dense vectors at all (None: lexical-only table):
/// callers gate the dense leg on it instead of assuming density.
pub struct SearchBackend {
    repo: Arc<CanonicalRepository>,
    table: SearchTable,
    embedder: Arc<dyn QueryEmbedder>,
    model_fingerprint: Option<ltmrs_domain::id::ModelFingerprint>,
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
            model_fingerprint: None,
        }
    }

    /// Declare the dense model this backend serves (E5 wiring); default None.
    pub fn with_model_fingerprint(
        mut self,
        fingerprint: ltmrs_domain::id::ModelFingerprint,
    ) -> Self {
        self.model_fingerprint = Some(fingerprint);
        self
    }

    /// The dense fingerprint, if this backend serves vectors.
    pub fn model_fingerprint(&self) -> Option<ltmrs_domain::id::ModelFingerprint> {
        self.model_fingerprint
    }

    /// The Lance table (for similarity candidate generation).
    pub fn table(&self) -> SearchTable {
        self.table.clone()
    }

    /// Lexical readiness state: Complete (converged), Partial (index
    /// missing or projection lagging — results are best-effort), or
    /// Unavailable (table unreadable). A usable backend returning zero
    /// hits is Complete-empty, never a reason to substitute other results.
    pub fn search_state(&self) -> SearchState {
        let fts = match self.fts_ready() {
            Err(e) => {
                return SearchState::Unavailable {
                    reason: format!("table unreadable: {}", e.message),
                };
            }
            Ok(ready) => ready,
        };
        if !fts {
            return SearchState::Partial {
                reason: "fts index not built".to_string(),
            };
        }
        match self.repo.projection_lag() {
            Err(e) => SearchState::Partial {
                reason: format!("pending queue unreadable: {}", e.message),
            },
            Ok(0) => SearchState::Complete,
            Ok(lag) => SearchState::Partial {
                reason: format!("{lag} projection jobs pending"),
            },
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
            ltmrs_domain::command::DomainError::new(
                ltmrs_domain::command::DomainErrorCode::Validation,
                format!("no tokio runtime for search: {e}"),
            )
        })?;
        let repo = self.repo.clone();
        let table = self.table.clone();
        let embedder = Arc::clone(&self.embedder);
        handle.block_on(async move {
            // Reopen to the latest dataset version: table handles are
            // snapshot-pinned, and the projection tick commits through its
            // own handle. Without this, serving would silently miss
            // tick-published rows and indexes until a daemon restart.
            // Bench and unit tests construct `Engine` directly and keep
            // full control of handle freshness themselves.
            let mut table = table;
            table.refresh().await?;
            let engine = Engine::new(repo, table);
            engine.retrieve(req, embedder.as_ref()).await
        })
    }

    /// Embed one query-role vector synchronously (bridges async via
    /// block_on, same runtime contract as `retrieve_sync`).
    pub fn embed_query_sync(&self, text: &str) -> DomainResult<Vec<f32>> {
        let handle = tokio::runtime::Handle::try_current().map_err(|e| {
            ltmrs_domain::command::DomainError::new(
                ltmrs_domain::command::DomainErrorCode::Validation,
                format!("no tokio runtime for search: {e}"),
            )
        })?;
        handle.block_on(self.embedder.embed_query(text))
    }

    /// Embed passage-role vectors synchronously (one bridge for the batch;
    /// embedders without passage support report unsupported and callers
    /// fall back to the token path).
    pub fn embed_passages_sync(&self, texts: &[String]) -> DomainResult<Vec<Vec<f32>>> {
        let handle = tokio::runtime::Handle::try_current().map_err(|e| {
            ltmrs_domain::command::DomainError::new(
                ltmrs_domain::command::DomainErrorCode::Validation,
                format!("no tokio runtime for search: {e}"),
            )
        })?;
        handle.block_on(self.embedder.embed_passages(texts))
    }

    /// Whether the FTS index is ready (for readiness reporting).
    pub fn fts_ready(&self) -> DomainResult<bool> {
        let handle = tokio::runtime::Handle::try_current().map_err(|e| {
            ltmrs_domain::command::DomainError::new(
                ltmrs_domain::command::DomainErrorCode::Validation,
                format!("no tokio runtime for search: {e}"),
            )
        })?;
        let table = self.table.clone();
        Ok(handle.block_on(async move { table.fts_index_ready().await.unwrap_or(false) }))
    }
}

/// Lexical readiness state of the attached table.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchState {
    /// Converged: results are authoritative, including empty ones.
    Complete,
    /// Best-effort: the FTS index is missing or projection lags. Results
    /// may be incomplete; the pending-projection overlay covers the lag for
    /// mutation preflights.
    Partial { reason: String },
    /// The table itself is unreadable: route to the degraded path.
    Unavailable { reason: String },
}

/// A query embedder for lexical-only backends: dense is unavailable, so
/// every embedding call fails and callers take their documented
/// no-dense path (the engine skips the dense leg without a fingerprint).
pub struct NoDenseEmbedder;

impl QueryEmbedder for NoDenseEmbedder {
    fn embed_query<'a>(
        &'a self,
        _query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(DomainError::new(
                DomainErrorCode::Validation,
                "dense unavailable: lexical-only backend",
            ))
        })
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
            ltmrs_domain::command::DomainError::new(
                ltmrs_domain::command::DomainErrorCode::Validation,
                "embedder lock poisoned",
            )
        })?;
        guard.embed(query).map_err(|e| {
            ltmrs_domain::command::DomainError::new(
                ltmrs_domain::command::DomainErrorCode::Validation,
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

    fn embed_texts(&mut self, texts: &[String]) -> Vec<Result<Vec<f32>, String>> {
        use ltmrs_embeddings::e5_small::EmbedInput;
        use ltmrs_embeddings::recipe::Role;

        if texts.is_empty() {
            return Vec::new();
        }
        let inputs: Vec<EmbedInput> = texts
            .iter()
            .map(|text| EmbedInput {
                text: text.clone(),
                role: Role::Passage,
            })
            .collect();
        match self.embed_batch(&inputs) {
            Ok(seqs) => {
                let mut out: Vec<Result<Vec<f32>, String>> =
                    seqs.into_iter().map(|s| Ok(s.vector)).collect();
                // Defensive length match: never silently drop or pad units.
                while out.len() < texts.len() {
                    out.push(Err("batch returned fewer sequences than inputs".to_string()));
                }
                out.truncate(texts.len());
                out
            }
            Err(e) => texts.iter().map(|_| Err(e.to_string())).collect(),
        }
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
    service: ltmrs_embeddings::service::EmbeddingService,
}

impl ServiceQueryEmbedder {
    pub fn new(service: ltmrs_embeddings::service::EmbeddingService) -> Self {
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
                    ltmrs_embeddings::worker::ServiceError::Busy => {
                        format!("busy: {message}")
                    }
                    _ => format!("query embedding failed: {message}"),
                };
                DomainError::new(DomainErrorCode::Validation, message)
            })
        })
    }

    /// Passage-role batch embed over the bounded worker (one round-trip for
    /// the whole catalog; error mapping mirrors the query path, including
    /// the `busy:` backpressure prefix).
    fn embed_passages<'a>(
        &'a self,
        texts: &'a [String],
    ) -> crate::retrieval::engine::PassageVectorsFuture<'a> {
        let service = self.service.clone();
        let inputs: Vec<ltmrs_embeddings::e5_small::EmbedInput> = texts
            .iter()
            .map(|text| ltmrs_embeddings::e5_small::EmbedInput {
                text: text.clone(),
                role: Role::Passage,
            })
            .collect();
        Box::pin(async move {
            service
                .embed_batch(inputs)
                .await
                .map(|seqs| seqs.into_iter().map(|s| s.vector).collect())
                .map_err(|e| {
                    let message = e.to_string();
                    let message = match &e {
                        ltmrs_embeddings::worker::ServiceError::Busy => {
                            format!("busy: {message}")
                        }
                        _ => format!("passage embedding failed: {message}"),
                    };
                    DomainError::new(DomainErrorCode::Validation, message)
                })
        })
    }
}

/// Projector embedder over a shared adapter handle: per-tick projector
/// rebuilds (generation freshness) clone the `Arc` without reloading
/// weights. Ticks are sequential so the mutex is uncontended in practice;
/// a poisoned lock fails the tick loudly (retry next tick), never silently.
impl Embedder for Arc<Mutex<E5SmallAdapter>> {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        let mut guard = self
            .lock()
            .map_err(|_| "embedding adapter lock poisoned".to_string())?;
        (&mut *guard as &mut dyn Embedder).embed(text)
    }

    fn embed_texts(&mut self, texts: &[String]) -> Vec<Result<Vec<f32>, String>> {
        match self.lock() {
            Ok(mut guard) => (&mut *guard as &mut dyn Embedder).embed_texts(texts),
            Err(_) => texts
                .iter()
                .map(|_| Err("embedding adapter lock poisoned".to_string()))
                .collect(),
        }
    }

    fn chunk_text(&self, title: &str, fragment: &str) -> Vec<TextChunk> {
        match self.lock() {
            Ok(guard) => e5_chunks_to_text_chunks(title, &guard.chunk_passage(title, fragment)),
            // Poisoned by a panicked tick: stay lexically indexed with the
            // default single unit rather than dropping the memory.
            Err(_) => vec![crate::search::projector::single_chunk_unit(title, fragment)],
        }
    }

    fn chunker_version(&self) -> String {
        E5_CHUNK_VERSION.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltmrs_embeddings::e5_small::Chunk;

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
        use ltmrs_embeddings::artifacts::ArtifactResult;
        use ltmrs_embeddings::e5_small::{EmbedInput, EmbeddedSequence};
        use ltmrs_embeddings::recipe::Role;
        use ltmrs_embeddings::service::EmbeddingService;
        use ltmrs_embeddings::worker::{EmbeddingWorkerConfig, SyncEmbedder};
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
        use ltmrs_embeddings::service::EmbeddingService;
        use ltmrs_embeddings::worker::EmbeddingWorkerConfig;

        struct EmptyEmbedder;
        impl ltmrs_embeddings::worker::SyncEmbedder for EmptyEmbedder {
            fn embed_batch(
                &mut self,
                inputs: &[ltmrs_embeddings::e5_small::EmbedInput],
            ) -> ltmrs_embeddings::artifacts::ArtifactResult<
                Vec<ltmrs_embeddings::e5_small::EmbeddedSequence>,
            > {
                Ok(inputs
                    .iter()
                    .map(|_| ltmrs_embeddings::e5_small::EmbeddedSequence {
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
        use crate::retrieval::engine::QueryEmbedder;
        use ltmrs_embeddings::service::EmbeddingService;
        use ltmrs_embeddings::worker::{EmbeddingWorkerConfig, SyncEmbedder};

        struct SlowEmbedder;
        impl SyncEmbedder for SlowEmbedder {
            fn embed_batch(
                &mut self,
                inputs: &[ltmrs_embeddings::e5_small::EmbedInput],
            ) -> ltmrs_embeddings::artifacts::ArtifactResult<
                Vec<ltmrs_embeddings::e5_small::EmbeddedSequence>,
            > {
                std::thread::sleep(std::time::Duration::from_millis(500));
                Ok(inputs
                    .iter()
                    .map(|_| ltmrs_embeddings::e5_small::EmbeddedSequence {
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
                svc.embed("first", ltmrs_embeddings::recipe::Role::Query)
                    .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let w2 = tokio::spawn({
            let svc = svc.clone();
            async move {
                svc.embed("second", ltmrs_embeddings::recipe::Role::Query)
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
        use crate::retrieval::engine::RetrievalRequest;
        use ltmrs_embeddings::service::EmbeddingService;
        use ltmrs_embeddings::worker::{EmbeddingWorkerConfig, SyncEmbedder};

        struct ConstEmbedder;
        impl SyncEmbedder for ConstEmbedder {
            fn embed_batch(
                &mut self,
                inputs: &[ltmrs_embeddings::e5_small::EmbedInput],
            ) -> ltmrs_embeddings::artifacts::ArtifactResult<
                Vec<ltmrs_embeddings::e5_small::EmbeddedSequence>,
            > {
                CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(inputs
                    .iter()
                    .map(|_| ltmrs_embeddings::e5_small::EmbeddedSequence {
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
            ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
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
            model_fingerprint: Some(ltmrs_domain::id::ModelFingerprint::new(1)),
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

    /// search_state tracks table readiness through the same bridge contract
    /// as retrieve_sync (blocking context): fresh table without an index
    /// reports Partial, never Complete.
    #[tokio::test]
    async fn search_state_partial_without_fts_index() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = std::sync::Arc::new(
            ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
                .unwrap(),
        );
        let lance_dir = tempfile::tempdir().unwrap();
        let table = crate::search::table::SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let backend = SearchBackend::new(repo, table, std::sync::Arc::new(NoDenseEmbedder));
        let state = tokio::task::spawn_blocking(move || backend.search_state())
            .await
            .unwrap();
        assert!(
            matches!(&state, SearchState::Partial { reason } if reason.contains("fts")),
            "unindexed table must report Partial, got: {state:?}"
        );
    }

    /// search_state reports Complete once the index is built and no
    /// projection work pends: a converged empty table is authoritative,
    /// including for empty answers.
    #[tokio::test]
    async fn search_state_complete_when_converged() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = std::sync::Arc::new(
            ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
                .unwrap(),
        );
        let lance_dir = tempfile::tempdir().unwrap();
        let table = crate::search::table::SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        table.ensure_fts_index().await.unwrap();
        let backend = SearchBackend::new(repo, table, std::sync::Arc::new(NoDenseEmbedder));
        let state = tokio::task::spawn_blocking(move || backend.search_state())
            .await
            .unwrap();
        assert_eq!(state, SearchState::Complete);
    }

    /// Live E5 embedding against a provisioned cache (ignored: needs the
    /// ~500MB pinned artifacts). Gate: `LTMRS_PROBE_MODELS=<models dir>`;
    /// skips (never fails) without it so the suite stays offline-safe.
    /// Proves the serving-time embed path — adapter load + worker + sync
    /// bridge, query and passage roles — independent of table contents.
    #[tokio::test]
    #[ignore]
    async fn live_e5_embed_against_provisioned_cache() {
        use ltmrs_embeddings::artifacts::ArtifactCache;
        use ltmrs_embeddings::service::EmbeddingService;

        let models = std::env::var("LTMRS_PROBE_MODELS").unwrap_or_default();
        if models.is_empty() || !std::path::Path::new(&models).exists() {
            eprintln!("SKIP: set LTMRS_PROBE_MODELS to a provisioned models dir");
            return;
        }
        let cache = ArtifactCache::new(&models);
        let svc =
            EmbeddingService::load_e5_small_from_cache(&cache).expect("provisioned cache loads");
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = std::sync::Arc::new(
            ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
                .unwrap(),
        );
        let table_dir = tempfile::tempdir().unwrap();
        let table = crate::search::table::SearchTable::open(table_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let backend = SearchBackend::new(
            repo,
            table,
            std::sync::Arc::new(ServiceQueryEmbedder::new(svc)),
        );
        // Same sync-context rule as the dispatcher: bridge from blocking code.
        let (q, p) = tokio::task::spawn_blocking(move || {
            let q = backend.embed_query_sync("fox jumping near a river")?;
            let p = backend.embed_passages_sync(&[
                "the quick brown fox jumps over the lazy dog".to_string(),
            ])?;
            Ok::<_, ltmrs_domain::command::DomainError>((q, p))
        })
        .await
        .unwrap()
        .unwrap();
        for (role, v) in [("query", &q), ("passage", &p[0])] {
            assert_eq!(v.len(), 384, "{role} dim");
            assert!(v.iter().all(|x| x.is_finite()), "{role} finite");
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-4,
                "{role} L2-normalized, got {norm}"
            );
        }
        assert_ne!(q, p[0], "query/passage roles differ (prefixes applied)");

        // The projection seam over a shared adapter handle: same vector
        // space, E5 chunk policy stamp, single unit for short text.
        use crate::search::projector::Embedder as _;
        use ltmrs_embeddings::e5_small::E5SmallAdapter;
        let adapter = E5SmallAdapter::load_from_cache(&cache)
            .expect("provisioned cache loads for projection");
        let mut shared = std::sync::Arc::new(std::sync::Mutex::new(adapter));
        let pv = shared
            .embed("the quick brown fox jumps over the lazy dog")
            .unwrap();
        assert_eq!(pv.len(), 384, "projection dim");
        assert!(pv.iter().all(|x| x.is_finite()), "projection finite");
        let units = shared.chunk_text("T", "short fragment");
        assert_eq!(units.len(), 1, "short text is one unit");
        assert_eq!(units[0].text, "T\nshort fragment");
        assert_eq!(shared.chunker_version(), E5_CHUNK_VERSION);
    }

    /// Batched E5 embedding matches sequential embedding within float
    /// tolerance on fixed texts (padding masks make the math identical;
    /// batch dim may reorder reductions). Ignored: needs pinned artifacts.
    #[tokio::test]
    #[ignore]
    async fn e5_batch_matches_sequential_within_tolerance() {
        use crate::search::projector::Embedder;
        use ltmrs_embeddings::artifacts::ArtifactCache;
        use ltmrs_embeddings::e5_small::E5SmallAdapter;

        let models = std::env::var("LTMRS_PROBE_MODELS").unwrap_or_default();
        if models.is_empty() || !std::path::Path::new(&models).exists() {
            eprintln!("SKIP: set LTMRS_PROBE_MODELS to a provisioned models dir");
            return;
        }
        let cache = ArtifactCache::new(&models);
        let mut adapter = E5SmallAdapter::load_from_cache(&cache).expect("provisioned cache loads");
        let texts = vec![
            "the quick brown fox jumps over the lazy dog".to_string(),
            "quantum entanglement enables instantaneous correlation".to_string(),
        ];
        let batched = adapter
            .embed_texts(&texts)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let single0 = Embedder::embed(&mut adapter, &texts[0]).unwrap();
        let single1 = Embedder::embed(&mut adapter, &texts[1]).unwrap();
        for (b, s) in batched.iter().zip([single0, single1].iter()) {
            assert_eq!(b.len(), s.len());
            for (x, y) in b.iter().zip(s.iter()) {
                assert!((x - y).abs() < 1e-5, "batched diverged: {x} vs {y}");
            }
        }
        // Production tick shape: the projector boxes the shared
        // `Arc<Mutex<E5SmallAdapter>>` handle, so the batch override must
        // fire through the wrapper's `embed_texts` forward as well.
        let shared = std::sync::Arc::new(std::sync::Mutex::new(
            E5SmallAdapter::load_from_cache(&cache).expect("provisioned cache loads"),
        ));
        let mut boxed: Box<dyn Embedder> = Box::new(std::sync::Arc::clone(&shared));
        let wrapped = boxed
            .embed_texts(&texts)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(wrapped.len(), batched.len());
        for (w, b) in wrapped.iter().zip(batched.iter()) {
            assert_eq!(w.len(), b.len());
            for (x, y) in w.iter().zip(b.iter()) {
                assert!((x - y).abs() < 1e-5, "wrapper diverged: {x} vs {y}");
            }
        }
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

    /// Serving freshness: `retrieve_sync` observes commits made after the
    /// backend was constructed (production: the tick publishes through its
    /// own handle; serving must not pin a stale snapshot).
    #[tokio::test]
    async fn retrieve_sync_sees_post_construction_commits() {
        use crate::retrieval::engine::RetrievalRequest;
        use crate::search::row::SearchRow;
        use ltmrs_domain::id::ModelFingerprint;

        let repo_dir = tempfile::tempdir().unwrap();
        let repo = std::sync::Arc::new(
            ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
                .unwrap(),
        );
        let table_dir = tempfile::tempdir().unwrap();
        let uri = table_dir.path().to_str().unwrap();
        let table = crate::search::table::SearchTable::open(uri).await.unwrap();
        let backend = std::sync::Arc::new(SearchBackend::new(
            repo,
            table,
            std::sync::Arc::new(QueryEmbedderAdapter::new(std::sync::Arc::new(
                ClosureEmbedder::new(|_| Ok(vec![1.0; 384])),
            ))),
        ));
        // A second handle commits a dense row after construction.
        let writer = crate::search::table::SearchTable::open(uri).await.unwrap();
        writer
            .publish_rows(&[SearchRow {
                store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
                memory_id: ltmrs_domain::id::EntityId::new(uuid::Uuid::from_u128(1)),
                document_revision: ltmrs_domain::id::DocumentRevision::new(1),
                model_fingerprint: ModelFingerprint::new(1),
                chunk_id: ltmrs_domain::id::ChunkId::new(0),
                chunker_version: "single-chunk-v1".to_string(),
                lexical_text: "fresh row".to_string(),
                char_start: 0,
                char_end: 9,
                project: None,
                fragment_type: "fact".to_string(),
                created_at_millis: 1,
                confidence: 0.5,
                updated_at_millis: 1,
                embedding: Some(vec![1.0; 384]),
            }])
            .await
            .unwrap();
        let req = RetrievalRequest {
            query: "fresh".to_string(),
            model_fingerprint: Some(ModelFingerprint::new(1)),
            ..Default::default()
        };
        // Same sync-context rule as the dispatcher: bridge from blocking code.
        let out = tokio::task::spawn_blocking(move || backend.retrieve_sync(&req))
            .await
            .unwrap()
            .unwrap();
        assert!(
            out.explanation.dense_ready,
            "serving must observe the committed dense row"
        );
    }
}
