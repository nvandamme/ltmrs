//! Scheduled index optimization and retention (WP-05 task 10).
//!
//! A single background worker runs budgeted Lance maintenance on an interval,
//! keeping the search projection compact and bounded without starving
//! interactive work. Every action is constrained by an explicit
//! `MaintenanceBudget`; snapshot protection keeps a version alive while any
//! reader may still hold it.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::search::table::{MaintenanceBudget, SearchTable};
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::projection::GenerationStatus;
use ltmrs_service::repository::CanonicalRepository;

/// Configuration for the maintenance scheduler's schedule and budgets.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceConfig {
    /// How often a full optimization/retention pass runs.
    pub interval: Duration,
    /// Explicit resource budgets applied to every pass.
    pub budget: MaintenanceBudget,
    /// Retired-generation retention before the reaper deletes rows.
    /// Fresh-retired rows stay for rollback until this age.
    pub generation_retain_millis: u64,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(300),
            budget: MaintenanceBudget::default(),
            generation_retain_millis: 7 * 24 * 60 * 60 * 1000, // 7 days
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
    repo: Option<Arc<CanonicalRepository>>,
}

impl MaintenanceScheduler {
    pub fn new(table: SearchTable, config: MaintenanceConfig) -> Self {
        Self {
            table,
            config,
            repo: None,
        }
    }

    /// Attach the canonical repository so passes also reap expired retired
    /// generations. Without it, passes only optimize (generation rows stay).
    pub fn with_repo(mut self, repo: Arc<CanonicalRepository>) -> Self {
        self.repo = Some(repo);
        self
    }

    /// Run one maintenance pass (optimization + retention under the budget).
    /// Exposed separately so tests and shutdown can drive it deterministically.
    pub async fn run_pass(&self) -> DomainResult<PassOutcome> {
        maintenance_pass(&self.table, &self.config, self.repo.as_deref()).await
    }

    /// Spawn the background loop. The returned handle must be aborted on daemon
    /// shutdown so no orphaned worker survives (design §7.2). Passes run
    /// sequentially: if one is slow, the next tick simply waits for it to finish.
    pub fn spawn(&self) -> JoinHandle<()> {
        let table = self.table.clone();
        let config = self.config;
        let repo = self.repo.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(config.interval).await;
                if let Err(e) = maintenance_pass(&table, &config, repo.as_deref()).await {
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

/// One maintenance pass body shared by run_pass and the spawned loop:
/// budgeted optimization, then expired-retired-generation reaping where a
/// repository is attached. Snapshot protection is enforced inside
/// optimize_with_budgets: pruning never removes a version younger than
/// `retain_millis`, so any reader that opened the table within the
/// retention window keeps its files.
async fn maintenance_pass(
    table: &SearchTable,
    config: &MaintenanceConfig,
    repo: Option<&CanonicalRepository>,
) -> DomainResult<PassOutcome> {
    table.optimize_with_budgets(config.budget).await?;
    if let Some(repo) = repo {
        reap_retired_generations(
            table,
            repo,
            wall_now_millis(),
            config.generation_retain_millis,
        )
        .await?;
    }
    Ok(PassOutcome::Completed)
}

/// Wall-clock millis for retention measurements (production clock; tests
/// control retirement timestamps through the repository's frozen clock and
/// pass explicit `now` to the reaper directly).
fn wall_now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Reap expired retired generations (design §12.3 rollback retention): delete
/// every projected row of a Retired generation whose retirement age reached
/// `retain_millis`. Fresh-retired rows stay for rollback; the active
/// generation is never touched (it has no Retired record). Returns the number
/// of generations reaped. Safe against stale projectors: their rows are
/// refused by the publication guard, so a reaped generation cannot be
/// resurrected by a delayed worker.
pub async fn reap_retired_generations(
    table: &SearchTable,
    repo: &CanonicalRepository,
    now_millis: u64,
    retain_millis: u64,
) -> DomainResult<u64> {
    let mut reaped = 0u64;
    for rec in repo.list_generations()? {
        if rec.status != GenerationStatus::Retired {
            continue;
        }
        if now_millis.saturating_sub(rec.updated_at_millis) < retain_millis {
            continue;
        }
        let filter = format!("store_generation = {}", rec.generation.as_u64());
        table.delete_where(&filter).await?;
        reaped += 1;
    }
    Ok(reaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::projector::FixedEmbedder;
    use ltmrs_domain::command::DomainCommand;
    use ltmrs_domain::id::{DocumentRevision, EntityId, ModelFingerprint, StoreGeneration};
    use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
    use ltmrs_service::repository::CanonicalRepository;
    use std::sync::Arc;
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn ctx(op_num: u64) -> ltmrs_domain::command::CommandContext {
        ctx_gen(op_num, StoreGeneration::FIRST)
    }

    /// Context under an explicit generation (post-cutover writes name the
    /// live generation, exactly like a re-handshaked client; the
    /// transaction fence rejects retired generations).
    fn ctx_gen(op_num: u64, generation: StoreGeneration) -> ltmrs_domain::command::CommandContext {
        ltmrs_domain::command::CommandContext {
            store_generation: generation,
            frontend_id: ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1)),
            channel_id: ltmrs_domain::id::ChannelId::new(Uuid::from_u128(2)),
            session: None,
            operation_id: ltmrs_domain::id::OperationId::new(Uuid::from_u128(op_num as u128)),
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
            project_id: None,
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
            entity_revision: ltmrs_domain::id::EntityRevision::new(1),
            document_revision: DocumentRevision::new(1),
            eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(1),
            created_at: Instant::new(n * 10),
            updated_at: Instant::new(n * 10),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    async fn env() -> (Arc<CanonicalRepository>, SearchTable, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir_in(dir.path()).unwrap();
        let clock = Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        let repo_path = dir.path().to_str().unwrap();
        let repo = CanonicalRepository::open_with_clock(repo_path, clock).unwrap();
        let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
        assert_eq!(
            repo.issue_namespace(
                fe,
                ltmrs_domain::id::ChannelId::new(Uuid::from_u128(2)),
                1000
            )
            .unwrap()
            .retry_epoch,
            1
        );
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
                auto_link: None,
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
                generation_retain_millis: 3_600_000,
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
                auto_link: None,
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
                generation_retain_millis: MaintenanceConfig::default().generation_retain_millis,
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

    /// A wired scheduler reaps expired retired generations on every pass:
    /// retired-at is frozen-clock old, wall-clock now is far later, so the
    /// retention window has always elapsed in this setup.
    #[tokio::test]
    async fn run_pass_reaps_expired_retired_generations() {
        use ltmrs_domain::id::{ModelFingerprint as Fp, StoreGeneration as Gen};

        let (repo, table, _guard) = env().await;
        for n in [1u64, 2] {
            repo.apply(
                &ctx(n),
                &DomainCommand::AddMemory {
                    memory: memory(n),
                    session: None,
                    auto_link: None,
                },
            )
            .unwrap();
        }
        let mut p1 = crate::search::projector::Projector::new(
            Arc::clone(&repo),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            Fp::new(1),
            Gen::FIRST,
        );
        p1.run_until_idle().await.unwrap();
        assert_eq!(table.count_rows(None).await.unwrap(), 2);

        // Cut over: generation 1 retires at frozen t=1000.
        let gen2 = repo.stage_generation(Fp::new(2)).unwrap();
        repo.note_generation_progress(gen2, 2).unwrap();
        repo.activate_generation(gen2).unwrap();

        let sched = MaintenanceScheduler::new(table.clone(), MaintenanceConfig::default())
            .with_repo(Arc::clone(&repo));
        sched.run_pass().await.unwrap();
        assert_eq!(
            table
                .count_rows(Some("store_generation = 1"))
                .await
                .unwrap(),
            0,
            "expired retired generation must be reaped on a pass"
        );
        assert_eq!(repo.store_generation().unwrap(), gen2);
    }

    /// An unwired scheduler changes nothing about generations: the reaper
    /// only runs where a repository is attached.
    #[tokio::test]
    async fn run_pass_without_repo_skips_reaping() {
        use ltmrs_domain::id::{ModelFingerprint as Fp, StoreGeneration as Gen};

        let (repo, table, _guard) = env().await;
        repo.apply(
            &ctx(1),
            &DomainCommand::AddMemory {
                memory: memory(1),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
        let mut p1 = crate::search::projector::Projector::new(
            Arc::clone(&repo),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            Fp::new(1),
            Gen::FIRST,
        );
        p1.run_until_idle().await.unwrap();
        let gen2 = repo.stage_generation(Fp::new(2)).unwrap();
        repo.note_generation_progress(gen2, 1).unwrap();
        repo.activate_generation(gen2).unwrap();

        MaintenanceScheduler::new(table.clone(), MaintenanceConfig::default())
            .run_pass()
            .await
            .unwrap();
        assert_eq!(
            table
                .count_rows(Some("store_generation = 1"))
                .await
                .unwrap(),
            1,
            "unwired passes must not reap"
        );
    }

    /// The reaper deletes only expired retired generations: fresh-retired
    /// rows stay for rollback, and the active generation is never touched.
    #[tokio::test]
    async fn reaper_deletes_only_expired_retired_generations() {
        use ltmrs_domain::id::ModelFingerprint as Fp;
        use ltmrs_domain::id::StoreGeneration as Gen;

        let (repo, table, _guard) = env().await;
        for n in [1u64, 2] {
            repo.apply(
                &ctx(n),
                &DomainCommand::AddMemory {
                    memory: memory(n),
                    session: None,
                    auto_link: None,
                },
            )
            .unwrap();
        }
        // Project both memories at generation 1.
        let mut p1 = crate::search::projector::Projector::new(
            Arc::clone(&repo),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            Fp::new(1),
            Gen::FIRST,
        );
        p1.run_until_idle().await.unwrap();
        assert_eq!(table.count_rows(None).await.unwrap(), 2);

        // Cut over to generation 2 (retires generation 1 at frozen t=1000).
        let gen2 = repo.stage_generation(Fp::new(2)).unwrap();
        repo.note_generation_progress(gen2, 2).unwrap();
        repo.activate_generation(gen2).unwrap();

        // Before the retain period elapses: nothing reaped (rollback intact).
        assert_eq!(
            reap_retired_generations(&table, &repo, 1000, 60_000)
                .await
                .unwrap(),
            0
        );
        assert_eq!(table.count_rows(None).await.unwrap(), 2);

        // After expiry: generation-1 rows are gone, generation-2 rows (none
        // projected yet) untouched, and the active pointer is unaffected.
        // Project one memory at gen 2 first so the active space is non-empty.
        repo.apply(
            &ctx_gen(3, gen2),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch: ltmrs_domain::command::MemoryPatch {
                    fragment: Some("f-1-v2".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let mut p2 = crate::search::projector::Projector::new(
            Arc::clone(&repo),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            Fp::new(2),
            gen2,
        );
        p2.run_until_idle().await.unwrap();
        assert_eq!(table.count_rows(None).await.unwrap(), 3);

        assert_eq!(
            reap_retired_generations(&table, &repo, 1000 + 60_000 + 1, 60_000)
                .await
                .unwrap(),
            1,
            "exactly the expired retired generation is reaped"
        );
        assert_eq!(
            table
                .count_rows(Some("store_generation = 1"))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            table
                .count_rows(Some("store_generation = 2"))
                .await
                .unwrap(),
            1
        );
        assert_eq!(repo.store_generation().unwrap(), gen2);
    }

    /// Recursive byte size of a store directory (canonical Fjall dir or Lance
    /// dataset dir). Test-only helper for the soak growth checkpoint.
    fn dir_bytes(path: &std::path::Path) -> u64 {
        let mut total = 0u64;
        let mut stack = vec![path.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
        total
    }

    /// One deterministic maintenance pass for the soak: projector drain
    /// (publish + ack), budgeted Lance optimize, receipt retention sweep, and
    /// an explicit generation-reaper call. The scheduler's `run_pass` is
    /// deliberately NOT used: it stamps retention with wall-clock time, which
    /// would make the soak nondeterministic. Returns (jobs resolved,
    /// receipts collected, generations reaped).
    async fn soak_maintenance_pass(
        repo: &Arc<CanonicalRepository>,
        table: &SearchTable,
        projector: &mut crate::search::projector::Projector,
        retain_millis: u64,
        frozen_now: u64,
    ) -> DomainResult<(usize, usize, u64)> {
        let resolved = projector.run_until_idle().await?;
        table
            .optimize_with_budgets(MaintenanceBudget {
                compaction_threads: 1,
                max_compact_bytes_per_file: 1024 * 1024,
                retain_millis: 3_600_000, // 1 hour retention
            })
            .await?;
        let collected = repo.gc_expired(frozen_now)?;
        let reaped = reap_retired_generations(table, repo, frozen_now, retain_millis).await?;
        Ok((resolved, collected, reaped))
    }

    /// T-BENCH-03 steady-state soak with maintenance, then kill/reopen (RQ-23).
    ///
    /// Bounded deterministic soak — explicitly NOT a long-run clock-time soak:
    /// a seeded LCG (seed 0x9E3779B97F4A7C15, same generator family as the
    /// retired `fjall_soak_mixed_ops_then_kill`) drives N_OPS=5000 mixed
    /// canonical ops over a 60-key space — puts for fresh ids, updates,
    /// feedback, Delete-mode forgets — with a maintenance pass every 500 ops
    /// (projector drain, budgeted optimize, retention sweep, reaper call).
    /// Kill (drop every handle) at op 3500; reopen the same directories and
    /// continue to 5000; then drain, assert recovery, cut over to generation
    /// 2, reap generation 1, and re-establish gen-2 projection.
    ///
    /// Asserts: (a) recovered state matches the pre-kill expectation (exact
    /// per-memory title/fragment/lifecycle match over the full expected map,
    /// live-count match, plus durable-receipt spot checks at ops 1/1750/3500/
    /// 5000 — the ACK ledger); (b) no pending projection jobs remain once the
    /// retired generation's rows are reaped, so no stale worker can resurrect
    /// them; (c) store-size growth is reported and bounded in two windows —
    /// op-500→end (loose tripwire spanning the kill/reopen) and
    /// post-reopen→end (steady-state, no reopen inside); (d) zero unexpected
    /// errors (every op's preconditions hold by construction, so any Err is
    /// a real failure, counted and asserted).
    ///
    /// Size bound: every op writes fixed-size values (~200 B of text); each op
    /// costs at most one canonical record rewrite plus at most one Lance row
    /// version (drains coalesce to one pending job per memory; prune retains
    /// versions for 1 h, longer than the test). 8 KiB/op is ~40x the nominal
    /// payload yet fails loudly on any unbounded duplication (a per-op
    /// full-table rewrite would exceed it by orders of magnitude).
    ///
    /// Determinism: seeded RNG, FrozenClock(1000), explicit `now` for GC and
    /// the reaper. Wall clock is read once for the reported duration only,
    /// never for a test decision.
    ///
    /// RED: pure-additive test, no RED transcript kept — RED was the test not existing (this name matched 0 tests before the body was added).
    #[tokio::test]
    async fn soak_mixed_ops_with_maintenance_then_kill() {
        use ltmrs_domain::command::ForgetMode;
        use ltmrs_domain::command::MemoryPatch;

        const SEED: u64 = 0x9E3779B97F4A7C15;
        const N_OPS: u64 = 5000;
        const KEY_SPACE: u64 = 60;
        const MAINT_EVERY: u64 = 500;
        const KILL_AT: u64 = 3500;
        const FROZEN_NOW: u64 = 1000;
        const RETAIN_MILLIS: u64 = 60_000;
        const SIZE_PER_OP_BUDGET: u64 = 8 * 1024;

        let wall_start = std::time::Instant::now();

        // Sibling tempdirs (not nested): repo and Lance sizes are measured
        // independently, so neither checkpoint double-counts the other.
        let repo_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let repo_path = repo_dir.path().to_str().unwrap().to_string();
        let uri = lance_dir.path().to_str().unwrap().to_string();
        let clock = Arc::new(ltmrs_domain::clock::FrozenClock::new(FROZEN_NOW));
        let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));

        let mut repo =
            Arc::new(CanonicalRepository::open_with_clock(&repo_path, clock.clone()).unwrap());
        assert_eq!(
            repo.issue_namespace(
                fe,
                ltmrs_domain::id::ChannelId::new(Uuid::from_u128(2)),
                FROZEN_NOW
            )
            .unwrap()
            .retry_epoch,
            1
        );
        let mut table = SearchTable::open(&uri).await.unwrap();
        let mut projector = crate::search::projector::Projector::new(
            Arc::clone(&repo),
            table.clone(),
            Box::new(FixedEmbedder { dim: 32 }),
            ltmrs_domain::id::ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );

        struct Expected {
            title: String,
            fragment: String,
            live: bool,
        }
        let mut expected: std::collections::HashMap<u64, Expected> =
            std::collections::HashMap::new();
        let mut live: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        let mut next_fresh = KEY_SPACE + 1;
        let mut rng: u64 = SEED;
        let (mut n_put, mut n_upd, mut n_fb, mut n_del) = (0u64, 0u64, 0u64, 0u64);
        let mut unexpected_errors = 0u32;
        let mut size_checkpoint_a = 0u64;
        let mut size_checkpoint_a2 = 0u64;

        let mut step = 0u64;
        while step < N_OPS {
            step += 1;
            // Seeded LCG: deterministic schedule, no test-harness timing.
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442694820888963407);
            let roll = rng % 100;

            let outcome = if roll < 50 {
                // Update a live memory (fall back to put when none live yet).
                match live.iter().next().copied() {
                    Some(id) => {
                        let target = 1 + rng % (live.len() as u64);
                        let id = *live.iter().nth((target - 1) as usize).unwrap_or(&id);
                        let title = format!("S{id}-s{step}");
                        let fragment = format!("frag-S{id}-s{step}");
                        let r = repo.apply(
                            &ctx(step),
                            &DomainCommand::UpdateMemory {
                                id: eid(id),
                                expected_revision: None,
                                patch: MemoryPatch {
                                    title: Some(title.clone()),
                                    fragment: Some(fragment.clone()),
                                    ..Default::default()
                                },
                            },
                        );
                        if r.is_ok() {
                            n_upd += 1;
                            expected.insert(
                                id,
                                Expected {
                                    title,
                                    fragment,
                                    live: true,
                                },
                            );
                        }
                        r.map(|_| ())
                    }
                    None => {
                        let id = next_fresh;
                        next_fresh += 1;
                        let mut m = memory(id);
                        m.title = format!("S{id}-v1");
                        m.fragment = format!("frag-S{id}-v1");
                        let r = repo
                            .apply(
                                &ctx(step),
                                &DomainCommand::AddMemory {
                                    memory: m,
                                    session: None,
                                    auto_link: None,
                                },
                            )
                            .map(|_| ());
                        if r.is_ok() {
                            n_put += 1;
                            live.insert(id);
                            expected.insert(
                                id,
                                Expected {
                                    title: format!("S{id}-v1"),
                                    fragment: format!("frag-S{id}-v1"),
                                    live: true,
                                },
                            );
                        }
                        r
                    }
                }
            } else if roll < 70 {
                // Feedback on a live memory (skip when none live yet).
                match live.iter().next().copied() {
                    Some(id) => {
                        let target = 1 + rng % (live.len() as u64);
                        let id = *live.iter().nth((target - 1) as usize).unwrap_or(&id);
                        let r = repo
                            .apply(
                                &ctx(step),
                                &DomainCommand::Feedback {
                                    memory_id: eid(id),
                                    useful: step.is_multiple_of(2),
                                },
                            )
                            .map(|_| ());
                        if r.is_ok() {
                            n_fb += 1;
                        }
                        r
                    }
                    None => Ok(()),
                }
            } else if roll < 80 {
                // Put a fresh id.
                let id = next_fresh;
                next_fresh += 1;
                let mut m = memory(id);
                m.title = format!("S{id}-v1");
                m.fragment = format!("frag-S{id}-v1");
                let r = repo
                    .apply(
                        &ctx(step),
                        &DomainCommand::AddMemory {
                            memory: m,
                            session: None,
                            auto_link: None,
                        },
                    )
                    .map(|_| ());
                if r.is_ok() {
                    n_put += 1;
                    live.insert(id);
                    expected.insert(
                        id,
                        Expected {
                            title: format!("S{id}-v1"),
                            fragment: format!("frag-S{id}-v1"),
                            live: true,
                        },
                    );
                }
                r
            } else if roll < 90 {
                // Delete-mode forget of a live memory (skip when none live).
                match live.iter().next().copied() {
                    Some(id) => {
                        let target = 1 + rng % (live.len() as u64);
                        let id = *live.iter().nth((target - 1) as usize).unwrap_or(&id);
                        let r = repo
                            .apply(
                                &ctx(step),
                                &DomainCommand::Forget {
                                    id: eid(id),
                                    mode: ForgetMode::Delete,
                                },
                            )
                            .map(|_| ());
                        if r.is_ok() {
                            n_del += 1;
                            live.remove(&id);
                            if let Some(e) = expected.get_mut(&id) {
                                e.live = false;
                            }
                        }
                        r
                    }
                    None => Ok(()),
                }
            } else {
                // Second update band: keeps update pressure dominant.
                match live.iter().next().copied() {
                    Some(id) => {
                        let target = 1 + rng % (live.len() as u64);
                        let id = *live.iter().nth((target - 1) as usize).unwrap_or(&id);
                        let title = format!("S{id}-t{step}");
                        let fragment = format!("frag-S{id}-t{step}");
                        let r = repo.apply(
                            &ctx(step),
                            &DomainCommand::UpdateMemory {
                                id: eid(id),
                                expected_revision: None,
                                patch: MemoryPatch {
                                    title: Some(title.clone()),
                                    fragment: Some(fragment.clone()),
                                    ..Default::default()
                                },
                            },
                        );
                        if r.is_ok() {
                            n_upd += 1;
                            expected.insert(
                                id,
                                Expected {
                                    title,
                                    fragment,
                                    live: true,
                                },
                            );
                        }
                        r.map(|_| ())
                    }
                    None => Ok(()),
                }
            };
            if let Err(e) = outcome {
                unexpected_errors += 1;
                eprintln!(
                    "soak op {step} unexpected error: {} ({})",
                    e.code.as_str(),
                    e.message
                );
            }

            if step.is_multiple_of(MAINT_EVERY) || step == N_OPS {
                let (resolved, collected, reaped) =
                    soak_maintenance_pass(&repo, &table, &mut projector, RETAIN_MILLIS, FROZEN_NOW)
                        .await
                        .unwrap();
                eprintln!(
                    "soak maintenance at op {step}: {resolved} jobs resolved, \
                     {collected} receipts collected, {reaped} generations reaped"
                );
                if step == MAINT_EVERY {
                    size_checkpoint_a = dir_bytes(repo_dir.path()) + dir_bytes(lance_dir.path());
                    eprintln!(
                        "soak size checkpoint A at op {step}: repo {} bytes + lance {} bytes",
                        dir_bytes(repo_dir.path()),
                        dir_bytes(lance_dir.path())
                    );
                }
            }

            if step == KILL_AT {
                // Kill boundary: drop every handle (repo, table, projector),
                // then reopen the same directories and continue writing.
                drop(projector);
                drop(table);
                drop(repo);
                let reopened = Arc::new(
                    CanonicalRepository::open_with_clock(&repo_path, clock.clone()).unwrap(),
                );
                let reopened_table = SearchTable::open(&uri).await.unwrap();
                projector = crate::search::projector::Projector::new(
                    Arc::clone(&reopened),
                    reopened_table.clone(),
                    Box::new(FixedEmbedder { dim: 32 }),
                    ltmrs_domain::id::ModelFingerprint::new(1),
                    StoreGeneration::FIRST,
                );
                table = reopened_table;
                repo = reopened;
                size_checkpoint_a2 = dir_bytes(repo_dir.path()) + dir_bytes(lance_dir.path());
                eprintln!(
                    "soak kill/reopen at op {KILL_AT}; post-reopen size {size_checkpoint_a2} bytes \
                     (repo {} + lance {})",
                    dir_bytes(repo_dir.path()),
                    dir_bytes(lance_dir.path())
                );
            }
        }
        assert_eq!(
            unexpected_errors, 0,
            "soak must produce zero unexpected errors"
        );

        // Final drain + maintenance before asserting recovery.
        let (resolved, collected, reaped) =
            soak_maintenance_pass(&repo, &table, &mut projector, RETAIN_MILLIS, FROZEN_NOW)
                .await
                .unwrap();
        eprintln!(
            "soak final maintenance: {resolved} jobs resolved, \
             {collected} receipts collected, {reaped} generations reaped"
        );

        // (a) Recovered state matches the pre-kill expectation: exact
        // per-memory match over the full expected map plus live-count match.
        let snapshot = repo.export_snapshot().unwrap();
        assert_eq!(
            snapshot.memories.len(),
            expected.len(),
            "canonical memory count must match the expected map"
        );
        let live_count = snapshot
            .memories
            .iter()
            .filter(|m| m.lifecycle.is_recallable())
            .count();
        assert_eq!(
            live_count,
            live.len(),
            "recallable count must match the live set"
        );
        let by_id: std::collections::HashMap<_, _> =
            snapshot.memories.iter().map(|m| (m.id, m)).collect();
        for (id, exp) in &expected {
            let found = by_id
                .get(&eid(*id))
                .unwrap_or_else(|| panic!("memory {id} lost across soak/kill"));
            assert_eq!(
                &found.title, &exp.title,
                "last-write-wins violated for {id}"
            );
            assert_eq!(&found.fragment, &exp.fragment, "fragment mismatch for {id}");
            assert_eq!(
                found.lifecycle.is_recallable(),
                exp.live,
                "lifecycle mismatch for {id}"
            );
        }
        // ACK ledger spot checks: durable receipts exist across the kill
        // boundary (op 1, mid-soak, kill boundary, final op).
        for probe in [1u64, 1750, KILL_AT, N_OPS] {
            let receipt = repo
                .lookup_receipt(
                    StoreGeneration::FIRST,
                    fe,
                    1,
                    ltmrs_domain::id::OperationId::new(Uuid::from_u128(probe as u128)),
                )
                .unwrap();
            assert!(receipt.is_some(), "durable receipt missing for op {probe}");
        }
        // All projection work acknowledged before the generation cutover.
        assert!(
            repo.projection_jobs().unwrap().is_empty(),
            "no pending projection jobs may survive the final drain"
        );

        // Generation rotation post-recovery: cut over, reap the retired
        // generation's rows, and prove the index is clean for rotation.
        let live_now = live.len() as u64;
        let gen2 = repo
            .stage_generation(ltmrs_domain::id::ModelFingerprint::new(2))
            .unwrap();
        repo.note_generation_progress(gen2, live_now).unwrap();
        repo.activate_generation(gen2).unwrap();
        let reaped =
            reap_retired_generations(&table, &repo, FROZEN_NOW + RETAIN_MILLIS + 1, RETAIN_MILLIS)
                .await
                .unwrap();
        assert_eq!(reaped, 1, "exactly the retired generation must be reaped");
        assert_eq!(
            table
                .count_rows(Some("store_generation = 1"))
                .await
                .unwrap(),
            0,
            "retired generation rows must be gone after reaping"
        );
        // (b) No leaked pending projection jobs for the retired generation:
        // nothing pending that a stale worker could republish into it.
        assert!(
            repo.projection_jobs().unwrap().is_empty(),
            "no pending jobs may reference the retired generation"
        );
        // Rotation did not brick projection: one gen-2 touch publishes.
        if let Some(&id) = live.iter().next() {
            let title = "S-rotation-probe".to_string();
            repo.apply(
                &ctx_gen(N_OPS + 1, gen2),
                &DomainCommand::UpdateMemory {
                    id: eid(id),
                    expected_revision: None,
                    patch: MemoryPatch {
                        title: Some(title),
                        fragment: None,
                        ..Default::default()
                    },
                },
            )
            .unwrap();
            let mut projector2 = crate::search::projector::Projector::new(
                Arc::clone(&repo),
                table.clone(),
                Box::new(FixedEmbedder { dim: 32 }),
                ltmrs_domain::id::ModelFingerprint::new(2),
                gen2,
            );
            projector2.run_until_idle().await.unwrap();
            assert!(
                table
                    .count_rows(Some("store_generation = 2"))
                    .await
                    .unwrap()
                    >= 1,
                "gen-2 projection must publish after rotation"
            );
            assert!(
                repo.projection_jobs().unwrap().is_empty(),
                "gen-2 drain must leave no pending jobs"
            );
        }

        // (c) Store-size growth. Two windows: the full op-500→end window is a
        // loose tripwire (it spans the kill/reopen; checkpoint A is dominated
        // by a 64 MiB preallocated Fjall journal, `0.jnl`, reclaimed across
        // the reopen — the shrink is engine-internal journal lifecycle,
        // claimed as nothing more); the post-reopen→end window is the honest
        // steady-state number (no reopen inside it). Both are bounded by the
        // per-op ceiling below.
        let repo_bytes_b = dir_bytes(repo_dir.path());
        let lance_bytes_b = dir_bytes(lance_dir.path());
        let size_checkpoint_b = repo_bytes_b + lance_bytes_b;
        let growth = size_checkpoint_b.saturating_sub(size_checkpoint_a);
        let growth_budget = N_OPS * SIZE_PER_OP_BUDGET;
        let growth2 = size_checkpoint_b.saturating_sub(size_checkpoint_a2);
        let growth2_budget = (N_OPS - KILL_AT) * SIZE_PER_OP_BUDGET;
        eprintln!(
            "soak size checkpoint B: repo {repo_bytes_b} bytes + lance {lance_bytes_b} bytes"
        );
        eprintln!(
            "T-BENCH-03 soak: {N_OPS} ops (puts {n_put}, updates {n_upd}, \
             feedback {n_fb}, deletes {n_del}), seed {SEED:#x}, kill at {KILL_AT}, \
             size {size_checkpoint_a} -> {size_checkpoint_b} bytes (+{growth}, \
             budget {growth_budget}), post-reopen {size_checkpoint_a2} -> \
             {size_checkpoint_b} (+{growth2}, budget {growth2_budget}), elapsed {:?}",
            wall_start.elapsed()
        );
        assert!(
            growth <= growth_budget,
            "store growth {growth} bytes exceeds {growth_budget} budget"
        );
        assert!(
            growth2 <= growth2_budget,
            "steady-state growth {growth2} bytes exceeds {growth2_budget} budget"
        );
    }
}
