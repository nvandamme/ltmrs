//! Test/adapter query embedders (moved verbatim from `backend.rs`).

use std::sync::{Arc, Mutex};

use super::QueryEmbedderProvider;
use crate::retrieval::engine::QueryEmbedder;
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};

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
