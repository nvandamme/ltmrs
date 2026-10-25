//! Search backend for the daemon (WP-08): wraps the Lance search table and a
//! query embedder so the dispatcher can run lexical + dense retrieval.
//!
//! The dispatcher is synchronous (it runs on `spawn_blocking`), so async
//! search is bridged with `Handle::current().block_on()`. The embedder is a
//! seam: production wires the Candle E5-small service; tests use a
//! deterministic adapter.

use std::sync::Arc;

use crate::retrieval::engine::{Engine, QueryEmbedder, RetrievalRequest, RetrievalResult};
use crate::search::table::SearchTable;
use ltmrs_domain::command::DomainResult;
use ltmrs_service::repository::CanonicalRepository;

#[cfg(test)]
mod backend_tests;
pub mod e5;
pub mod embedders;

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

    /// Whether the FTS index is ready (for readiness reporting). Lance
    /// failures propagate (a corrupt table is `Unavailable`, never a
    /// healthy-absent `Partial`).
    pub fn fts_ready(&self) -> DomainResult<bool> {
        let handle = tokio::runtime::Handle::try_current().map_err(|e| {
            ltmrs_domain::command::DomainError::new(
                ltmrs_domain::command::DomainErrorCode::Validation,
                format!("no tokio runtime for search: {e}"),
            )
        })?;
        let table = self.table.clone();
        handle.block_on(async move { table.fts_index_ready().await })
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
