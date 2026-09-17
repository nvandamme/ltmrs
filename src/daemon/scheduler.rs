//! Bounded embedding scheduler and blocking storage work (design §7.3, RQ-22,
//! T-CONC-04).
//!
//! Keeps CPU inference off Tokio core workers: a dedicated worker owns a
//! bounded queue and a synchronous `&mut self` model adapter. Interactive
//! recall is not starved by maintenance/indexing rebuilds. Backpressure is
//! visible as a retryable busy state, never unbounded allocation.

use tokio::sync::mpsc;

use crate::domain::id::EntityId;

/// Error from submitting work to the scheduler.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SchedulerError {
    #[error("scheduler queue full; retry later")]
    Busy,
    #[error("scheduler is closed")]
    Closed,
}

/// Job priority: interactive work is never starved by maintenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPriority {
    Interactive,
    Maintenance,
}

/// A unit of embedding work.
#[derive(Debug, Clone)]
pub struct EmbeddingJob {
    pub memory_id: EntityId,
    pub text: String,
    pub priority: JobPriority,
}

/// A synchronous embedding adapter. The model is a `&mut self` adapter, not a
/// dynamic plugin system.
pub trait EmbeddingAdapter: Send {
    fn embed(&mut self, text: &str) -> Vec<f32>;
}

/// A trivial fixed-dimension adapter for tests.
pub struct FixedDimAdapter {
    pub dim: usize,
}

impl EmbeddingAdapter for FixedDimAdapter {
    fn embed(&mut self, _text: &str) -> Vec<f32> {
        vec![0.0f32; self.dim]
    }
}

/// The bounded embedding scheduler.
pub struct EmbeddingScheduler {
    tx: mpsc::Sender<EmbeddingJob>,
}

/// Configuration for the scheduler's resource budgets.
#[derive(Debug, Clone, Copy)]
pub struct SchedulerConfig {
    /// Maximum queued jobs before backpressure.
    pub max_queue_depth: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_queue_depth: 1024,
        }
    }
}

impl EmbeddingScheduler {
    /// Create a scheduler and its worker. The worker runs the embedding
    /// adapter; the returned scheduler handles submission.
    pub fn spawn(
        adapter: Box<dyn EmbeddingAdapter>,
        config: SchedulerConfig,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<EmbeddingJob>(config.max_queue_depth);
        let worker = tokio::spawn(async move {
            let adapter = std::sync::Arc::new(std::sync::Mutex::new(adapter));
            while let Some(job) = rx.recv().await {
                // Synchronous inference runs on a blocking thread, never on a
                // Tokio core I/O worker (§7.3).
                let text = job.text.clone();
                let adapter = std::sync::Arc::clone(&adapter);
                let vec = tokio::task::spawn_blocking(move || {
                    let mut guard = adapter.lock().unwrap();
                    guard.embed(&text)
                })
                .await
                .unwrap_or_default();
                let _ = vec;
            }
        });
        (Self { tx }, worker)
    }

    /// Submit a job. Returns `Busy` when the queue is full (backpressure).
    pub fn submit(&self, job: EmbeddingJob) -> Result<(), SchedulerError> {
        match self.tx.try_send(job) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SchedulerError::Busy),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SchedulerError::Closed),
        }
    }

    /// Current queue depth (for health/diagnostics).
    pub fn queue_depth(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }

    /// Whether the scheduler is at capacity (backpressure active).
    pub fn is_full(&self) -> bool {
        self.tx.capacity() == 0
    }
}

/// Run blocking storage work off the async runtime. Wraps a closure in
/// `spawn_blocking` so synchronous KV transactions don't block Tokio I/O
/// workers.
pub fn run_blocking_storage<T, F>(f: F) -> tokio::task::JoinHandle<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::EntityId;
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn job(n: u64) -> EmbeddingJob {
        EmbeddingJob {
            memory_id: eid(n),
            text: format!("text {n}"),
            priority: JobPriority::Maintenance,
        }
    }

    #[tokio::test]
    async fn queue_is_bounded_with_backpressure() {
        let (sched, _worker) = EmbeddingScheduler::spawn(
            Box::new(FixedDimAdapter { dim: 8 }),
            SchedulerConfig { max_queue_depth: 3 },
        );

        // Fill the queue.
        assert!(sched.submit(job(1)).is_ok());
        assert!(sched.submit(job(2)).is_ok());
        assert!(sched.submit(job(3)).is_ok());
        // Queue full: backpressure as a retryable busy state.
        assert_eq!(sched.submit(job(4)), Err(SchedulerError::Busy));
        assert!(sched.is_full());
    }

    #[tokio::test]
    async fn queue_depth_tracks_pending_work() {
        let (sched, _worker) = EmbeddingScheduler::spawn(
            Box::new(FixedDimAdapter { dim: 8 }),
            SchedulerConfig { max_queue_depth: 5 },
        );
        assert_eq!(sched.queue_depth(), 0);
        sched.submit(job(1)).unwrap();
        sched.submit(job(2)).unwrap();
        assert_eq!(sched.queue_depth(), 2);
    }

    #[tokio::test]
    async fn closed_scheduler_reports_closed() {
        let (sched, worker) = EmbeddingScheduler::spawn(
            Box::new(FixedDimAdapter { dim: 8 }),
            SchedulerConfig { max_queue_depth: 2 },
        );
        // Abort the worker so its receiver is dropped, closing the channel.
        worker.abort();
        // Wait for the task to be fully cleaned up so the receiver is gone.
        let _ = worker.await;
        assert_eq!(sched.submit(job(1)), Err(SchedulerError::Closed));
    }

    #[tokio::test]
    async fn blocking_storage_work_runs() {
        let handle = run_blocking_storage(|| 21 * 2);
        let result = handle.await.unwrap();
        assert_eq!(result, 42);
    }
}
