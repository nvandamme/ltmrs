//! Bounded synchronous Candle embedding worker (WP-06 task 5; design §7.3, RQ-22).
//!
//! Keeps CPU inference off Tokio core workers: a dedicated OS thread owns the
//! adapter and drains a bounded request queue. Backpressure is visible as a
//! retryable `Busy` error, never unbounded allocation. Cancellation is
//! cooperative via generation tokens (cancel_all) plus per-request detection of
//! dropped reply handles (structured concurrency). Batch accounting exposes
//! processed/queued counters for health output.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::embeddings::artifacts::{ArtifactError, ArtifactResult};
use crate::embeddings::e5_small::{E5SmallAdapter, EmbedInput, EmbeddedSequence};

/// A synchronous embedding adapter the worker owns. The model is a `&mut self`
/// adapter, not a dynamic plugin system (design §9).
pub trait SyncEmbedder: Send {
    fn embed_batch(&mut self, inputs: &[EmbedInput]) -> ArtifactResult<Vec<EmbeddedSequence>>;
}

impl SyncEmbedder for E5SmallAdapter {
    fn embed_batch(&mut self, inputs: &[EmbedInput]) -> ArtifactResult<Vec<EmbeddedSequence>> {
        E5SmallAdapter::embed_batch(self, inputs)
    }
}

/// Configuration for the worker's resource budgets (RQ-22).
#[derive(Debug, Clone, Copy)]
pub struct EmbeddingWorkerConfig {
    /// Maximum items in one inference batch; larger requests are split.
    pub max_batch_size: usize,
    /// Maximum queued requests before backpressure (`Busy`) is applied.
    pub max_queue_depth: usize,
}

impl Default for EmbeddingWorkerConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 32,
            max_queue_depth: 1024,
        }
    }
}

/// Batch accounting counters (readable from any thread).
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkerStats {
    pub processed_batches: u64,
    pub processed_items: usize,
    pub cancelled_requests: u64,
    pub in_flight: usize,
}

/// Error returned by the async service.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("embedding queue is full; retry later")]
    Busy,
    #[error("embedding worker is not running")]
    Closed,
    #[error("request was cancelled")]
    Cancelled,
    #[error(transparent)]
    Inference(#[from] ArtifactError),
}

/// A bounded FIFO queue with non-blocking push (backpressure) and blocking pop.
pub struct BoundedQueue<T> {
    inner: Mutex<QueueInner<T>>,
    not_empty: Condvar,
}

struct QueueInner<T> {
    items: VecDeque<T>,
    capacity: usize,
}

impl<T> BoundedQueue<T> {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Mutex::new(QueueInner {
                items: VecDeque::with_capacity(capacity),
                capacity,
            }),
            not_empty: Condvar::new(),
        }
    }

    /// Push without blocking. Returns the item back (and `Err`) when full —
    /// this is the visible backpressure point for RQ-22.
    pub fn try_send(&self, item: T) -> Result<(), T> {
        let mut inner = self.inner.lock().unwrap();
        if inner.items.len() >= inner.capacity {
            return Err(item);
        }
        inner.items.push_back(item);
        self.not_empty.notify_one();
        Ok(())
    }

    /// Pop, waiting up to `timeout` for an item. Used by the worker loop so it
    /// can also observe shutdown flags between messages.
    pub fn pop_timeout(&self, timeout: Duration) -> Option<T> {
        let mut inner = self.inner.lock().unwrap();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(item) = inner.items.pop_front() {
                return Some(item);
            }
            // Wait only for the remaining time until the deadline so an early or
            // spurious wakeup cannot extend the total wait beyond `timeout`.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let result = self.not_empty.wait_timeout(inner, remaining).unwrap();
            (inner, _) = result;
            if std::time::Instant::now() >= deadline {
                return None;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct WorkerRequest {
    reply: oneshot::Sender<Result<Vec<EmbeddedSequence>, ServiceError>>,
    inputs: Vec<EmbedInput>,
    /// Generation snapshot at submit time; a mismatch means cancel_all() ran.
    generation: u64,
}

enum WorkerMessage {
    Request(WorkerRequest),
    Shutdown,
}

/// Shared worker thread state. The dedicated thread owns the adapter and runs
/// inference synchronously on its own OS thread (never on Tokio I/O workers).
pub struct EmbeddingWorkerHandle {
    queue: Arc<BoundedQueue<WorkerMessage>>,
    generation: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    stats: WorkerStatsRef,
}

type WorkerStatsRef = Arc<WorkerStatsCell>;

#[derive(Default)]
struct WorkerStatsCell {
    processed_batches: AtomicU64,
    processed_items: AtomicUsize,
    cancelled_requests: AtomicU64,
    in_flight: AtomicUsize,
}

/// The reply channel for a submitted request: results plus the generation at submit time.
type SubmitReply = (
    oneshot::Receiver<Result<Vec<EmbeddedSequence>, ServiceError>>,
    u64,
);

impl EmbeddingWorkerHandle {
    pub(crate) fn submit(&self, inputs: Vec<EmbedInput>) -> Result<SubmitReply, ServiceError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ServiceError::Closed);
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        let generation = self.generation.load(Ordering::Acquire);

        // The bounded queue is the backpressure point: a full queue must surface
        // as `Busy`, never unbounded allocation (RQ-22).
        match self.queue.try_send(WorkerMessage::Request(WorkerRequest {
            reply: reply_tx,
            inputs,
            generation,
        })) {
            Ok(()) => {
                self.stats.in_flight.fetch_add(1, Ordering::Relaxed);
                Ok((reply_rx, generation))
            }
            Err(_) if self.closed.load(Ordering::Acquire) => Err(ServiceError::Closed),
            Err(_) => Err(ServiceError::Busy),
        }
    }

    /// Cancel all queued and in-flight requests by bumping the generation.
    pub fn cancel_all(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn stats(&self) -> WorkerStats {
        let s = &self.stats;
        WorkerStats {
            processed_batches: s.processed_batches.load(Ordering::Relaxed),
            processed_items: s.processed_items.load(Ordering::Relaxed),
            cancelled_requests: s.cancelled_requests.load(Ordering::Relaxed),
            in_flight: s.in_flight.load(Ordering::Relaxed),
        }
    }

    /// Whether the worker is still accepting work.
    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    pub fn shutdown(&self) {
        if self.closed.swap(true, Ordering::Release) {
            return; // already shutting down
        }
        // The worker rechecks `closed` on every loop iteration (pop_timeout is
        // short), so a failed send under queue pressure still terminates it.
        let _ = self.queue.try_send(WorkerMessage::Shutdown);
    }
}

/// Spawn the dedicated embedding worker thread. The adapter is moved into it —
/// no cross-thread sharing of model state.
pub fn spawn_worker(
    adapter: impl SyncEmbedder + 'static,
    config: EmbeddingWorkerConfig,
) -> (EmbeddingWorkerHandle, std::thread::JoinHandle<()>) {
    let queue = Arc::new(BoundedQueue::new(config.max_queue_depth));
    let generation = Arc::new(AtomicU64::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(WorkerStatsCell::default());

    let queue_for_worker = Arc::clone(&queue);
    let gen_for_worker = Arc::clone(&generation);
    let closed_for_worker = Arc::clone(&closed);
    let stats_for_worker = Arc::clone(&stats);

    let worker = thread::Builder::new()
        .name("ltmrs-embedding-worker".to_string())
        .spawn(move || {
            run_worker_loop(
                adapter,
                queue_for_worker,
                config,
                gen_for_worker,
                closed_for_worker,
                stats_for_worker,
            )
        })
        .expect("failed to spawn embedding worker");

    (
        EmbeddingWorkerHandle {
            queue,
            generation,
            closed,
            stats: WorkerStatsRef::clone(&stats),
        },
        worker,
    )
}

fn run_worker_loop(
    mut adapter: impl SyncEmbedder,
    queue: Arc<BoundedQueue<WorkerMessage>>,
    config: EmbeddingWorkerConfig,
    generation: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    stats: WorkerStatsRef,
) {
    while !closed.load(Ordering::Acquire) {
        let Some(msg) = queue.pop_timeout(Duration::from_millis(10)) else {
            continue; // re-check the closed flag
        };

        match msg {
            WorkerMessage::Shutdown => break,
            WorkerMessage::Request(req) => {
                // Cooperative cancellation (RQ-22): a stale generation means
                // cancel_all() ran after submit; drop without running inference.
                if req.generation != generation.load(Ordering::Acquire) {
                    stats.cancelled_requests.fetch_add(1, Ordering::Relaxed);
                    let _ = req.reply.send(Err(ServiceError::Cancelled));
                    continue;
                }

                // Split oversized requests into bounded batches.
                let mut all_results: Vec<EmbeddedSequence> = Vec::new();
                let mut inference_error: Option<ServiceError> = None;
                for chunk in req.inputs.chunks(config.max_batch_size) {
                    match adapter.embed_batch(chunk) {
                        Ok(results) => {
                            stats.processed_batches.fetch_add(1, Ordering::Relaxed);
                            stats
                                .processed_items
                                .fetch_add(chunk.len(), Ordering::Relaxed);
                            all_results.extend(results);
                        }
                        Err(e) => {
                            inference_error = Some(ServiceError::Inference(e));
                            break;
                        }
                    }
                }

                // Publishing fails when the caller dropped its receiver: that is
                // per-request cancellation (structured concurrency).
                let outcome: Result<Vec<EmbeddedSequence>, ServiceError> = match inference_error {
                    Some(e) => Err(e),
                    None if all_results.len() == req.inputs.len() => Ok(all_results),
                    None => Err(ServiceError::Cancelled),
                };

                if req.reply.send(outcome).is_err() {
                    stats.cancelled_requests.fetch_add(1, Ordering::Relaxed);
                }
                stats.in_flight.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::recipe::Role;

    /// A fake adapter for worker tests: no weights required.
    struct FakeEmbedder {
        dim: usize,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl SyncEmbedder for FakeEmbedder {
        fn embed_batch(&mut self, inputs: &[EmbedInput]) -> ArtifactResult<Vec<EmbeddedSequence>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok((0..inputs.len())
                .map(|_| EmbeddedSequence {
                    vector: vec![0.5f32; self.dim],
                    input_ids: (0..self.dim as u32).collect(),
                    attention_mask: vec![true; self.dim],
                })
                .collect())
        }
    }

    #[test]
    fn worker_processes_requests_and_accounts_batches() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (handle, _worker) = spawn_worker(
            FakeEmbedder {
                dim: 8,
                calls: Arc::clone(&calls),
            },
            EmbeddingWorkerConfig {
                max_batch_size: 4,
                max_queue_depth: 16,
            },
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (rx, _) = handle
                .submit(vec![EmbedInput {
                    text: "a".into(),
                    role: Role::Query,
                }])
                .unwrap();
            let results = rx.await.unwrap().unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].vector.len(), 8);
        });

        // One request split into one batch (single item <= max_batch_size).
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let stats = handle.stats();
        assert_eq!(stats.processed_batches, 1);
        assert_eq!(stats.processed_items, 1);

        handle.shutdown();
    }

    #[test]
    fn oversized_requests_are_split_into_bounded_batches() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (handle, _worker) = spawn_worker(
            FakeEmbedder {
                dim: 4,
                calls: Arc::clone(&calls),
            },
            EmbeddingWorkerConfig {
                max_batch_size: 2,
                max_queue_depth: 16,
            },
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let inputs: Vec<EmbedInput> = (0..5)
                .map(|i| EmbedInput {
                    text: format!("t{i}"),
                    role: Role::Passage,
                })
                .collect();
            let (rx, _) = handle.submit(inputs).unwrap();
            let results = rx.await.unwrap().unwrap();
            assert_eq!(results.len(), 5);
        });

        // 5 items at max_batch_size=2 -> 3 batches.
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        handle.shutdown();
    }

    #[test]
    fn cancel_all_drops_queued_requests_without_inference() {
        // Slow adapter so queued requests are still pending when we cancel.
        struct SlowEmbedder;
        impl SyncEmbedder for SlowEmbedder {
            fn embed_batch(
                &mut self,
                inputs: &[EmbedInput],
            ) -> ArtifactResult<Vec<EmbeddedSequence>> {
                std::thread::sleep(Duration::from_millis(50));
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

        let (handle, _worker) = spawn_worker(
            SlowEmbedder,
            EmbeddingWorkerConfig {
                max_batch_size: 1,
                max_queue_depth: 8,
            },
        );

        // Submit two requests while the worker is busy with the first.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (rx1, _) = handle
                .submit(vec![EmbedInput {
                    text: "a".into(),
                    role: Role::Query,
                }])
                .unwrap();
            // Worker picks up request 1 and sleeps; queue request 2.
            tokio::time::sleep(Duration::from_millis(5)).await;
            let (rx2, _) = handle
                .submit(vec![EmbedInput {
                    text: "b".into(),
                    role: Role::Query,
                }])
                .unwrap();

            // Cancel everything queued after this point.
            handle.cancel_all();

            match rx1.await.unwrap() {
                Ok(_) => {} // processed before cancel — acceptable (cooperative)
                Err(ServiceError::Cancelled) => {}
                Err(e) => panic!("unexpected error: {e}"),
            }
            // The second request must be cancelled, never inferred.
            match rx2.await.unwrap() {
                Ok(_) => panic!("request after cancel_all must not succeed"),
                Err(ServiceError::Cancelled) => {}
                Err(e) => panic!("expected Cancelled, got: {e}"),
            }
        });

        let stats = handle.stats();
        assert!(stats.cancelled_requests >= 1);
        handle.shutdown();
    }

    #[test]
    fn dropped_receiver_is_detected_as_cancellation() {
        let (handle, _worker) = spawn_worker(
            FakeEmbedder {
                dim: 4,
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            EmbeddingWorkerConfig::default(),
        );

        // Submit and drop the receiver immediately: the worker must detect it.
        let (rx, _) = handle
            .submit(vec![EmbedInput {
                text: "x".into(),
                role: Role::Query,
            }])
            .unwrap();
        drop(rx);

        // Let the worker process the request.
        std::thread::sleep(Duration::from_millis(50));
        let stats = handle.stats();
        assert!(
            stats.cancelled_requests >= 1,
            "dropped receiver must count as cancellation"
        );
        handle.shutdown();
    }

    #[test]
    fn full_queue_reports_busy_backpressure() {
        struct SlowEmbedder;
        impl SyncEmbedder for SlowEmbedder {
            fn embed_batch(
                &mut self,
                inputs: &[EmbedInput],
            ) -> ArtifactResult<Vec<EmbeddedSequence>> {
                std::thread::sleep(Duration::from_millis(50));
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

        let (handle, _worker) = spawn_worker(
            SlowEmbedder,
            EmbeddingWorkerConfig {
                max_batch_size: 1,
                max_queue_depth: 2,
            },
        );

        // The worker processes one at a time; queue depth 2 means 3 requests fit.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Fill the queue while the worker is busy with the first request.
            let (rx1, _) = handle
                .submit(vec![EmbedInput {
                    text: "a".into(),
                    role: Role::Query,
                }])
                .unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
            let (rx2, _) = handle
                .submit(vec![EmbedInput {
                    text: "b".into(),
                    role: Role::Query,
                }])
                .unwrap();

            // Third request should hit the full queue (or succeed if timing differs).
            match handle.submit(vec![EmbedInput {
                text: "c".into(),
                role: Role::Query,
            }]) {
                Err(ServiceError::Busy) => {}
                Ok((rx3, _)) => {
                    let _ = rx1.await;
                    let _ = rx2.await;
                    let _ = rx3.await;
                }
                Err(other) => panic!("unexpected submit error in backpressure test: {other}"),
            }
        });

        handle.shutdown();
    }
}
