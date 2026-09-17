//! Health/doctor output (design §7.1). Reports readiness, store, projection and
//! resource budgets WITHOUT dumping memory contents (privacy, RQ-20).

use crate::daemon::limits::QuotaTracker;
use crate::daemon::scheduler::EmbeddingScheduler;
use crate::domain::id::StoreGeneration;

/// A readiness probe result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthReport {
    /// Whether the daemon is ready to serve.
    pub ready: bool,
    /// The store generation being served.
    pub store_generation: StoreGeneration,
    /// The protocol version.
    pub protocol_version: u32,
    /// Whether the search projection is current.
    pub projection_current: bool,
    /// Number of registered clients.
    pub client_count: usize,
    /// Total queued embedding jobs.
    pub queued_jobs: usize,
    /// In-flight storage operations.
    pub in_flight_storage: usize,
    /// Whether backpressure is active (queue full).
    pub backpressure_active: bool,
}

/// Build a health report from the daemon's components.
pub fn build_health_report(
    ready: bool,
    store_generation: StoreGeneration,
    protocol_version: u32,
    projection_current: bool,
    scheduler: &EmbeddingScheduler,
    quotas: &QuotaTracker,
) -> HealthReport {
    HealthReport {
        ready,
        store_generation,
        protocol_version,
        projection_current,
        client_count: quotas.client_count(),
        queued_jobs: quotas.total_queued(),
        in_flight_storage: quotas.in_flight_storage(),
        backpressure_active: scheduler.is_full(),
    }
}

/// A human-readable doctor summary (no memory contents).
pub fn doctor_summary(report: &HealthReport) -> String {
    format!(
        "ltmrs doctor\n  ready: {}\n  store_generation: {}\n  protocol: v{}\n  projection_current: {}\n  clients: {}\n  queued_jobs: {}\n  in_flight_storage: {}\n  backpressure_active: {}",
        report.ready,
        report.store_generation.as_u64(),
        report.protocol_version,
        report.projection_current,
        report.client_count,
        report.queued_jobs,
        report.in_flight_storage,
        report.backpressure_active,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::limits::ResourceLimits;
    use crate::daemon::scheduler::{EmbeddingJob, FixedDimAdapter, JobPriority, SchedulerConfig};
    use crate::domain::id::EntityId;
    use uuid::Uuid;

    #[tokio::test]
    async fn health_report_reflects_state() {
        let (sched, _worker) = EmbeddingScheduler::spawn(
            Box::new(FixedDimAdapter { dim: 8 }),
            SchedulerConfig { max_queue_depth: 4 },
        );
        let qt = QuotaTracker::new(ResourceLimits::default());

        let report = build_health_report(true, StoreGeneration::FIRST, 1, true, &sched, &qt);
        assert!(report.ready);
        assert_eq!(report.client_count, 0);
        assert!(!report.backpressure_active);

        // Fill the scheduler to trigger backpressure.
        for i in 0..4 {
            sched
                .submit(EmbeddingJob {
                    memory_id: EntityId::new(Uuid::from_u128(i as u128)),
                    text: format!("t{i}"),
                    priority: JobPriority::Maintenance,
                })
                .unwrap();
        }
        let report2 = build_health_report(true, StoreGeneration::FIRST, 1, true, &sched, &qt);
        assert!(report2.backpressure_active);
    }

    #[test]
    fn doctor_summary_has_no_memory_contents() {
        let report = HealthReport {
            ready: true,
            store_generation: StoreGeneration::FIRST,
            protocol_version: 1,
            projection_current: true,
            client_count: 3,
            queued_jobs: 5,
            in_flight_storage: 2,
            backpressure_active: false,
        };
        let summary = doctor_summary(&report);
        // Only aggregate metrics, no memory text.
        assert!(summary.contains("clients: 3"));
        assert!(summary.contains("queued_jobs: 5"));
    }
}
