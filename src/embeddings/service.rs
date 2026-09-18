//! Async embedding service (WP-06 task 5; design §9).
//!
//! Wraps the synchronous worker with an async API for callers on the Tokio
//! runtime. Submits work to the bounded worker thread and awaits completion,
//! keeping CPU inference off core I/O workers while remaining fully cancellable
//! through structured concurrency (dropping the future cancels the request).

use std::sync::Arc;

use crate::embeddings::artifacts::{ArtifactCache, ArtifactError};
use crate::embeddings::e5_small::{E5SmallAdapter, EmbedInput, EmbeddedSequence};
use crate::embeddings::worker::{
    EmbeddingWorkerConfig, EmbeddingWorkerHandle, ServiceError, SyncEmbedder,
};

/// The async embedding service. Cloneable — all handles share the same worker.
#[derive(Clone)]
pub struct EmbeddingService {
    handle: Arc<EmbeddingWorkerHandle>,
}

impl Drop for EmbeddingService {
    fn drop(&mut self) {
        // When the last service handle is dropped, shut down the worker thread.
        if Arc::strong_count(&self.handle) == 1 {
            self.handle.shutdown();
        }
    }
}

impl EmbeddingService {
    /// Create a new service backed by a dedicated worker thread running the
    /// given adapter (synchronous, `&mut self`). The adapter is moved into the
    /// worker — no cross-thread sharing.
    pub fn spawn(adapter: impl SyncEmbedder + 'static, config: EmbeddingWorkerConfig) -> Self {
        let (handle, _worker) = crate::embeddings::worker::spawn_worker(adapter, config);
        // The JoinHandle is intentionally dropped; the worker thread runs for the
        // lifetime of all service handles. Shutdown goes through `shutdown()` or Drop.
        std::mem::forget(_worker);
        Self {
            handle: Arc::new(handle),
        }
    }

    /// Create a service from an existing worker handle (for testing or shared workers).
    pub fn from_handle(handle: EmbeddingWorkerHandle) -> Self {
        Self {
            handle: Arc::new(handle),
        }
    }

    /// Embed a single text with the given role. Returns the normalized vector.
    pub async fn embed(
        &self,
        text: &str,
        role: crate::embeddings::recipe::Role,
    ) -> Result<Vec<f32>, ServiceError> {
        let inputs = vec![EmbedInput {
            text: text.to_string(),
            role,
        }];
        Ok(self.embed_batch(inputs).await?.pop().unwrap().vector)
    }

    /// Embed a batch of inputs. Returns one vector per input, all normalized and finite-checked.
    pub async fn embed_batch(
        &self,
        inputs: Vec<EmbedInput>,
    ) -> Result<Vec<EmbeddedSequence>, ServiceError> {
        let (rx, _gen) = self.handle.submit(inputs)?;

        // Awaiting `rx` on a tokio runtime is non-blocking for I/O workers.
        // Dropping this future (cancellation) drops `rx`, which the worker
        // detects as request cancellation when publishing fails.
        let result = rx.await.map_err(|_| ServiceError::Cancelled)??;
        Ok(result)
    }

    /// Cancel all queued and in-flight requests on this service.
    pub fn cancel_all(&self) {
        self.handle.cancel_all();
    }

    /// Batch accounting stats (RQ-22 health output).
    pub fn stats(&self) -> crate::embeddings::worker::WorkerStats {
        self.handle.stats()
    }

    /// Shut down the worker thread. Subsequent requests return `Closed`.
    pub fn shutdown(&self) {
        self.handle.shutdown();
    }

    /// Whether the service is still accepting work (for health checks).
    #[allow(dead_code)] // used by daemon diagnostics in later WPs
    pub fn is_available(&self) -> bool {
        match self.handle.submit(vec![]) {
            Ok(_) => true,
            Err(ServiceError::Closed) => false,
            _ => true, // Busy still means available
        }
    }

    /// Load the E5-small adapter from cache and spawn a service.
    pub fn load_e5_small_from_cache(cache: &ArtifactCache) -> Result<Self, ArtifactError> {
        let adapter = E5SmallAdapter::load_from_cache(cache)?;
        Ok(Self::spawn(adapter, EmbeddingWorkerConfig::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::recipe::Role;

    /// A fast fake adapter for service tests.
    struct FastEmbedder {
        dim: usize,
    }

    impl SyncEmbedder for FastEmbedder {
        fn embed_batch(
            &mut self,
            inputs: &[EmbedInput],
        ) -> crate::embeddings::artifacts::ArtifactResult<Vec<EmbeddedSequence>> {
            Ok(inputs
                .iter()
                .map(|_| EmbeddedSequence {
                    vector: vec![1.0f32 / (self.dim as f32).sqrt(); self.dim],
                    input_ids: vec![42u32; 5],
                    attention_mask: vec![true; 5],
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn service_embeds_single_text() {
        let svc =
            EmbeddingService::spawn(FastEmbedder { dim: 16 }, EmbeddingWorkerConfig::default());

        let vec = svc.embed("hello world", Role::Query).await.unwrap();
        assert_eq!(vec.len(), 16);
        // Unit-normalized.
        let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
        assert!(
            (norm_sq - 1.0).abs() < 1e-5,
            "vector must be unit-normalized"
        );

        svc.shutdown();
    }

    #[tokio::test]
    async fn service_embeds_batch() {
        let svc =
            EmbeddingService::spawn(FastEmbedder { dim: 8 }, EmbeddingWorkerConfig::default());

        let inputs = vec![
            EmbedInput {
                text: "a".into(),
                role: Role::Query,
            },
            EmbedInput {
                text: "b".into(),
                role: Role::Passage,
            },
            EmbedInput {
                text: "c".into(),
                role: Role::Query,
            },
        ];

        let results = svc.embed_batch(inputs).await.unwrap();
        assert_eq!(results.len(), 3);
        for r in &results {
            assert!(!r.vector.is_empty());
            assert!(r.vector.iter().all(|v| v.is_finite()));
        }

        // Stats reflect the work.
        let stats = svc.stats();
        assert_eq!(stats.processed_items, 3);

        svc.shutdown();
    }

    #[tokio::test]
    async fn service_cancellation_via_drop() {
        struct SlowEmbedder;
        impl SyncEmbedder for SlowEmbedder {
            fn embed_batch(
                &mut self,
                inputs: &[EmbedInput],
            ) -> crate::embeddings::artifacts::ArtifactResult<Vec<EmbeddedSequence>> {
                std::thread::sleep(std::time::Duration::from_millis(100));
                Ok(inputs
                    .iter()
                    .map(|_| EmbeddedSequence {
                        vector: vec![1.0],
                        input_ids: vec![],
                        attention_mask: vec![],
                    })
                    .collect())
            }
        }

        let svc = EmbeddingService::spawn(SlowEmbedder, EmbeddingWorkerConfig::default());

        // Start an embed but cancel it by dropping the future.
        let inner_svc = svc.clone();
        let handle = tokio::spawn(async move { inner_svc.embed("long text", Role::Query).await });

        // Give the worker time to pick up the request, then abort.
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        handle.abort();

        // Wait for the worker to notice the cancellation.
        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

        let stats = svc.stats();
        assert!(
            stats.cancelled_requests >= 1,
            "aborted request must count as cancelled"
        );
        svc.shutdown();
    }

    #[tokio::test]
    async fn service_reports_stats() {
        let svc =
            EmbeddingService::spawn(FastEmbedder { dim: 4 }, EmbeddingWorkerConfig::default());

        // Initial stats are zero.
        let initial = svc.stats();
        assert_eq!(initial.processed_items, 0);

        // Do some work.
        for _ in 0..3 {
            svc.embed("test", Role::Passage).await.unwrap();
        }

        let final_stats = svc.stats();
        assert_eq!(final_stats.processed_items, 3);
        assert_eq!(final_stats.processed_batches, 3);

        svc.shutdown();
    }
}
