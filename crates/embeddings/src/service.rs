//! Async embedding service (WP-06 task 5; design §9).
//!
//! Wraps the synchronous worker with an async API for callers on the Tokio
//! runtime. Submits work to the bounded worker thread and awaits completion,
//! keeping CPU inference off core I/O workers while remaining fully cancellable
//! through structured concurrency (dropping the future cancels the request).

use std::sync::Arc;

use crate::artifacts::{ArtifactCache, ArtifactError};
use crate::e5_small::{E5SmallAdapter, EmbedInput, EmbeddedSequence};
use crate::worker::{EmbeddingWorkerConfig, EmbeddingWorkerHandle, ServiceError, SyncEmbedder};

/// Joint worker ownership: the service handle plus the worker thread's
/// `JoinHandle`. Last-ownership is determined atomically by `Arc` itself —
/// a check-then-act `strong_count` test here would race when two final
/// clones drop concurrently (both can observe 2 and both skip shutdown).
/// `Inner::drop` signals shutdown; the detached thread exits on its own
/// (dropping a `JoinHandle` detaches — no `mem::forget` leak).
struct ServiceInner {
    handle: EmbeddingWorkerHandle,
    _worker: std::thread::JoinHandle<()>,
}

impl Drop for ServiceInner {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}

/// The async embedding service. Cloneable — all handles share the same worker.
#[derive(Clone)]
pub struct EmbeddingService {
    inner: Arc<ServiceInner>,
}

impl EmbeddingService {
    /// Create a new service backed by a dedicated worker thread running the
    /// given adapter (synchronous, `&mut self`). The adapter is moved into the
    /// worker — no cross-thread sharing.
    pub fn spawn(adapter: impl SyncEmbedder + 'static, config: EmbeddingWorkerConfig) -> Self {
        let (handle, worker) = crate::worker::spawn_worker(adapter, config);
        Self {
            inner: Arc::new(ServiceInner {
                handle,
                _worker: worker,
            }),
        }
    }

    /// Create a service that jointly owns an existing worker thread (for
    /// testing or shared workers): the passed `JoinHandle` is kept until
    /// the last clone drops, which shuts the worker down.
    pub fn from_handle(handle: EmbeddingWorkerHandle, worker: std::thread::JoinHandle<()>) -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                handle,
                _worker: worker,
            }),
        }
    }

    fn handle(&self) -> &EmbeddingWorkerHandle {
        &self.inner.handle
    }

    /// Embed a single text with the given role. Returns the normalized vector.
    pub async fn embed(
        &self,
        text: &str,
        role: crate::recipe::Role,
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
        let (rx, _gen) = self.handle().submit(inputs)?;

        // Awaiting `rx` on a tokio runtime is non-blocking for I/O workers.
        // Dropping this future (cancellation) drops `rx`, which the worker
        // detects as request cancellation when publishing fails.
        let result = rx.await.map_err(|_| ServiceError::Cancelled)??;
        Ok(result)
    }

    /// Cancel all queued and in-flight requests on this service.
    pub fn cancel_all(&self) {
        self.handle().cancel_all();
    }

    /// Batch accounting stats (RQ-22 health output).
    pub fn stats(&self) -> crate::worker::WorkerStats {
        self.handle().stats()
    }

    /// Shut down the worker thread. Subsequent requests return `Closed`.
    pub fn shutdown(&self) {
        self.handle().shutdown();
    }

    /// Whether the service is still accepting work (for health checks).
    /// Observes worker state only: never submits, never perturbs stats.
    #[allow(dead_code)] // used by daemon diagnostics in later WPs
    pub fn is_available(&self) -> bool {
        self.handle().is_open()
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
    use crate::recipe::Role;

    /// A fast fake adapter for service tests.
    struct FastEmbedder {
        dim: usize,
    }

    impl SyncEmbedder for FastEmbedder {
        fn embed_batch(
            &mut self,
            inputs: &[EmbedInput],
        ) -> crate::artifacts::ArtifactResult<Vec<EmbeddedSequence>> {
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

    /// Health checks observe worker state without submitting work: no queue
    /// slots consumed, no stats perturbed.
    #[tokio::test]
    async fn availability_check_has_no_side_effects() {
        let svc =
            EmbeddingService::spawn(FastEmbedder { dim: 16 }, EmbeddingWorkerConfig::default());
        assert!(svc.is_available());
        assert!(svc.is_available());
        let stats = svc.handle().stats();
        assert_eq!(stats.processed_batches, 0, "health checks must not embed");
        assert_eq!(stats.in_flight, 0, "health checks must not occupy slots");
        svc.shutdown();
        assert!(!svc.is_available(), "shutdown service is unavailable");
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
            ) -> crate::artifacts::ArtifactResult<Vec<EmbeddedSequence>> {
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

    /// Dropping the last service clone closes the worker (no explicit
    /// shutdown call needed).
    #[test]
    fn last_drop_shuts_down_worker() {
        let svc =
            EmbeddingService::spawn(FastEmbedder { dim: 16 }, EmbeddingWorkerConfig::default());
        // Bare-handle observer: shares state, owns no worker lifetime.
        let raw = svc.handle().clone();
        let again = svc.clone();
        drop(svc);
        assert!(raw.is_open(), "worker stays open while clones live");
        drop(again);
        assert!(!raw.is_open(), "last drop must close the worker");
    }

    /// Concurrent last drops must still close the worker: exactly one of
    /// the two final clones owns the last reference — never neither
    /// (strong_count check-then-drop races when both observe 2).
    #[test]
    fn concurrent_last_drops_shut_down_worker() {
        for _ in 0..300 {
            let svc =
                EmbeddingService::spawn(FastEmbedder { dim: 16 }, EmbeddingWorkerConfig::default());
            let raw = svc.handle().clone();
            let a = svc.clone();
            let b = svc.clone();
            drop(svc);
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|s| {
                s.spawn(|| {
                    barrier.wait();
                    drop(a);
                });
                s.spawn(|| {
                    barrier.wait();
                    drop(b);
                });
            });
            assert!(
                !raw.is_open(),
                "worker must close after all clones dropped concurrently"
            );
        }
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
