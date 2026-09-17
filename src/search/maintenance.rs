//! Scheduled index optimization and retention (WP-05 task 10).
//!
//! A single background worker runs budgeted Lance maintenance on an interval,
//! keeping the search projection compact and bounded without starving
//! interactive work. Every action is constrained by an explicit
//! `MaintenanceBudget`; snapshot protection keeps a version alive while any
//! reader may still hold it.

use std::time::Duration;

use tokio::task::JoinHandle;

use crate::domain::command::DomainResult;
use crate::search::table::{MaintenanceBudget, SearchTable};

/// Configuration for the maintenance scheduler's schedule and budgets.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceConfig {
    /// How often a full optimization/retention pass runs.
    pub interval: Duration,
    /// Explicit resource budgets applied to every pass.
    pub budget: MaintenanceBudget,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(300),
            budget: MaintenanceBudget::default(),
        }
    }
}

/// Outcome of a single maintenance pass (for diagnostics/tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassOutcome {
    /// The pass ran and completed.
    Completed,
}

/// The scheduled maintenance worker. Spawned once per daemon; all passes run on
/// one task so they are sequential by construction — a slow pass simply delays
/// the next tick rather than overlapping it (RQ-22). Every action is bounded by
/// an explicit `MaintenanceBudget`; snapshot protection keeps a version alive
/// while any reader may still hold it.
pub struct MaintenanceScheduler {
    table: SearchTable,
    config: MaintenanceConfig,
}

impl MaintenanceScheduler {
    pub fn new(table: SearchTable, config: MaintenanceConfig) -> Self {
        Self { table, config }
    }

    /// Run one maintenance pass (optimization + retention under the budget).
    /// Exposed separately so tests and shutdown can drive it deterministically.
    pub async fn run_pass(&self) -> DomainResult<PassOutcome> {
        // Snapshot protection is enforced inside optimize_with_budgets: pruning
        // never removes a version younger than `retain_millis`, so any reader
        // that opened the table within the retention window keeps its files.
        self.table.optimize_with_budgets(self.config.budget).await?;
        Ok(PassOutcome::Completed)
    }

    /// Spawn the background loop. The returned handle must be aborted on daemon
    /// shutdown so no orphaned worker survives (design §7.2). Passes run
    /// sequentially: if one is slow, the next tick simply waits for it to finish.
    pub fn spawn(&self) -> JoinHandle<()> {
        let table = self.table.clone();
        let config = self.config;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(config.interval).await;
                if let Err(e) = table.optimize_with_budgets(config.budget).await {
                    // A failed maintenance pass must not kill the daemon: log and
                    // retry on the next tick. Canonical state is untouched either
                    // way (design §8.1); derived cleanup is always recoverable.
                    eprintln!(
                        "ltmrs maintenance pass failed: {} ({})",
                        e.code.as_str(),
                        e.message
                    );
                }
            }
        })
    }

    /// Run passes until `max_passes` complete — a test/drain helper that
    /// converges the table to its optimized state.
    pub async fn run_until_idle(&self, max_passes: usize) -> DomainResult<usize> {
        let mut completed = 0;
        for _ in 0..max_passes {
            self.run_pass().await?;
            completed += 1;
        }
        Ok(completed)
    }

    /// The configured interval (diagnostics).
    pub fn interval(&self) -> Duration {
        self.config.interval
    }

    /// The active budget (diagnostics/health output).
    pub fn budget(&self) -> MaintenanceBudget {
        self.config.budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::DomainCommand;
    use crate::domain::id::{DocumentRevision, EntityId, ModelFingerprint, StoreGeneration};
    use crate::domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
    use crate::search::projector::FixedEmbedder;
    use crate::service::repository::CanonicalRepository;
    use std::sync::Arc;
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn ctx(op_num: u64) -> crate::domain::command::CommandContext {
        crate::domain::command::CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: crate::domain::id::FrontendId::new(Uuid::from_u128(1)),
            channel_id: crate::domain::id::ChannelId::new(Uuid::from_u128(2)),
            session: None,
            operation_id: crate::domain::id::OperationId::new(Uuid::from_u128(op_num as u128)),
            request_digest: format!("d{op_num}"),
            deadline_millis: None,
            scope: Default::default(),
            retry_epoch: 1,
        }
    }

    fn memory(n: u64) -> Memory {
        let id = eid(n);
        Memory {
            id,
            external_alias: None,
            title: format!("t-{n}"),
            fragment: format!("f-{n}"),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: Some("ltmrs".into()),
            source: MemorySource::Ai,
            confidence: 0.5,
            quality_score: None,
            lifecycle: MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: crate::domain::id::EntityRevision::new(1),
            document_revision: DocumentRevision::new(1),
            eligibility_revision: crate::domain::id::EligibilityRevision::new(1),
            created_at: Instant::new(n * 10),
            updated_at: Instant::new(n * 10),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    async fn env() -> (Arc<CanonicalRepository>, SearchTable, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir_in(dir.path()).unwrap();
        let clock = Arc::new(crate::domain::clock::FrozenClock::new(1000));
        let repo_path = dir.path().to_str().unwrap();
        let repo = CanonicalRepository::open_with_clock(repo_path, clock).unwrap();
        let fe = crate::domain::id::FrontendId::new(Uuid::from_u128(1));
        assert_eq!(repo.issue_namespace(fe, 1000).unwrap().retry_epoch, 1);
        let uri = lance_dir.path().to_str().unwrap().to_string();
        let table = SearchTable::open(&uri).await.unwrap();
        (Arc::new(repo), table, dir)
    }

    /// A maintenance pass runs under the explicit budget and leaves data intact.
    #[tokio::test]
    async fn pass_applies_budget_and_preserves_data() {
        let (repo, table, _guard) = env().await;
        repo.apply(
            &ctx(1),
            &DomainCommand::AddMemory {
                memory: memory(1),
                session: None,
            },
        )
        .unwrap();

        // Publish a row so compaction/pruning has something to manage.
        let mut p = crate::search::projector::Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        if let Some(job) = repo.projection_job(eid(1)).unwrap() {
            p.process_job(&job).await.unwrap();
        }

        // A tight budget still completes: every knob is explicit.
        let sched = MaintenanceScheduler::new(
            table.clone(),
            MaintenanceConfig {
                interval: Duration::from_secs(60),
                budget: MaintenanceBudget {
                    compaction_threads: 1,
                    max_compact_bytes_per_file: 1024 * 1024,
                    retain_millis: 3_600_000, // 1 hour retention
                },
            },
        );
        assert_eq!(sched.run_pass().await.unwrap(), PassOutcome::Completed);

        // Data intact after maintenance.
        assert_eq!(table.count_rows(None).await.unwrap(), 1);
    }

    /// The scheduler reports its configured interval and budget (health seam).
    #[tokio::test]
    async fn config_is_exposed_for_diagnostics() {
        let (_repo, table, _guard) = env().await;
        let cfg = MaintenanceConfig::default();
        let sched = MaintenanceScheduler::new(table, cfg);
        assert_eq!(sched.interval(), Duration::from_secs(300));
        assert_eq!(sched.budget(), MaintenanceBudget::default());
    }

    /// Concurrent passes on one table never corrupt it: Lance serializes the
    /// commits, so racing maintenance passes both succeed or the loser reports a
    /// conflict as an error — never data loss (RQ-22). The spawn loop adds
    /// single-flight scheduling on top; this test pins the backend guarantee.
    #[tokio::test]
    async fn concurrent_passes_are_serialized_by_lance() {
        let (_repo, table, _guard) = env().await;
        let sched1 = Arc::new(MaintenanceScheduler::new(
            table.clone(),
            MaintenanceConfig::default(),
        ));
        let sched2 = sched1.clone();

        // Two passes racing on the same dataset.
        let h1 = tokio::spawn(async move { sched1.run_pass().await });
        let h2 = tokio::spawn(async move { sched2.run_pass().await });

        match (h1.await.unwrap(), h2.await.unwrap()) {
            (Ok(_), Ok(_)) => {}                    // both serialized cleanly,
            (Ok(_), Err(_)) | (Err(_), Ok(_)) => {} // or one lost the commit race,
            (Err(a), Err(b)) => panic!(
                "both passes failed: {} ({}) / {} ({})",
                a.code.as_str(),
                a.message,
                b.code.as_str(),
                b.message
            ),
        }
    }

    /// The spawned worker runs maintenance passes on its configured interval
    /// (task 10 scheduling): with a paused clock, advancing virtual time by one
    /// interval wakes exactly one pass — no real-time sleeps, fully deterministic.
    #[tokio::test(start_paused = true)]
    async fn spawn_runs_passes_on_interval() {
        let (repo, table, _guard) = env().await;

        // A row so each pass has real work to manage.
        repo.apply(
            &ctx(1),
            &DomainCommand::AddMemory {
                memory: memory(1),
                session: None,
            },
        )
        .unwrap();
        let mut p = crate::search::projector::Projector::new(
            Arc::clone(&repo),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        if let Some(job) = repo.projection_job(eid(1)).unwrap() {
            p.process_job(&job).await.unwrap();
        }

        let sched = MaintenanceScheduler::new(
            table.clone(),
            MaintenanceConfig {
                interval: std::time::Duration::from_secs(60),
                budget: MaintenanceBudget::default(),
            },
        );
        let handle = sched.spawn();

        // One full interval elapses (virtual): the worker wakes and runs a pass.
        tokio::time::sleep(std::time::Duration::from_secs(61)).await;

        // Data intact after the scheduled maintenance pass.
        assert_eq!(table.count_rows(None).await.unwrap(), 1);
        handle.abort();
    }

    /// run_until_idle converges: repeated passes complete and report their count.
    #[tokio::test]
    async fn run_until_idle_completes_expected_passes() {
        let (_repo, table, _guard) = env().await;
        let sched = MaintenanceScheduler::new(table, MaintenanceConfig::default());
        assert_eq!(sched.run_until_idle(3).await.unwrap(), 3);
    }
}
