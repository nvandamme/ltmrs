//! E5-small embedding bridge (moved verbatim from `backend.rs`).

use std::sync::{Arc, Mutex};

use super::QueryEmbedderProvider;
use crate::retrieval::engine::QueryEmbedder;
use crate::search::projector::{Embedder, TextChunk};
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_embeddings::e5_small::{Chunk, E5SmallAdapter, EmbedInput};
use ltmrs_embeddings::recipe::Role;

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
