//! Search backend for the daemon (WP-08): wraps the Lance search table and a
//! query embedder so the dispatcher can run lexical + dense retrieval.
//!
//! The dispatcher is synchronous (it runs on `spawn_blocking`), so async
//! search is bridged with `Handle::current().block_on()`. The embedder is a
//! seam: production wires the Candle E5-small service; tests use a
//! deterministic adapter.

use std::sync::{Arc, Mutex};

use crate::domain::command::DomainResult;
use crate::retrieval::engine::{Engine, QueryEmbedder, RetrievalRequest, RetrievalResult};
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
/// `RetrievalRequest::model_fingerprint`, so the backend holds no model state.
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
