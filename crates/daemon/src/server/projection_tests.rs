//! Projection-worker tests (moved verbatim from `server.rs`).

use std::sync::Arc;

use super::test_support::*;
use super::{Daemon, DaemonConfig, ProjectionTrigger};
use crate::runtime::RuntimePaths;
use ltmrs_domain::clock::{Clock, FrozenClock};
use ltmrs_domain::command::Scope;
use ltmrs_domain::id::{OperationId, StoreGeneration};
use ltmrs_service::repository::CanonicalRepository;
use uuid::Uuid;

/// P2-1 wake-up: a commit notification returns from the wait immediately
/// (no interval sleep); without one the wait spans the full interval.
/// Paused clock: fully deterministic, no real-time sleeps.
#[tokio::test(start_paused = true)]
async fn projection_trigger_wakes_on_commit_not_interval() {
    let trigger = ProjectionTrigger::new();
    let mut wait = Box::pin(trigger.wait(std::time::Duration::from_secs(300)));
    // Probe at t=0 (starts the interval timer), then elapse 299s: the
    // wait must still be pending — zero-duration timeouts probe liveness
    // without moving the clock.
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(0), &mut wait)
            .await
            .is_err()
    );
    tokio::time::advance(std::time::Duration::from_secs(299)).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(0), &mut wait)
            .await
            .is_err(),
        "without a wake the wait must span the full interval"
    );
    // The 300th second completes the interval wait.
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::time::timeout(std::time::Duration::from_secs(1), &mut wait)
        .await
        .expect("interval expiry must still fire the wait");
    // A commit wake fires a fresh wait without any clock advance.
    trigger.wake();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        trigger.wait(std::time::Duration::from_secs(300)),
    )
    .await
    .expect("commit wake must fire immediately");
}

/// Re-review R6: more than two batch limits of pending work keeps
/// draining across back-to-back bounded passes with no new commits and
/// no interval wait. Deterministic: no clock advance between batches.
#[tokio::test]
async fn projection_drains_past_one_batch_without_sleep() {
    use ltmrs_domain::command::DomainCommand;
    use ltmrs_search::search::projector::FixedEmbedder;
    use ltmrs_search::search::table::SearchTable;

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(
            dir.path().join("store").to_str().unwrap(),
            Arc::clone(&clock),
        )
        .unwrap(),
    );
    repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
    let table_dir = dir.path().join("table");
    std::fs::create_dir_all(&table_dir).unwrap();
    let table = SearchTable::open(table_dir.to_str().unwrap())
        .await
        .unwrap();
    // Seed 250 pending jobs (2.5 batch limits at MAX=100).
    for n in 1..=250u64 {
        let ctx = ltmrs_domain::command::CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe(1),
            channel_id: ch(1),
            session: None,
            operation_id: OperationId::new(Uuid::from_u128(1000 + n as u128)),
            request_digest: format!("drain-test-{n}"),
            deadline_millis: None,
            scope: Scope::default(),
            retry_epoch: 1,
        };
        repo.apply(
            &ctx,
            &DomainCommand::AddMemory {
                memory: test_memory(n),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    assert_eq!(repo.projection_lag().unwrap(), 250);
    // Three consecutive bounded batches, no writes and no clock advance
    // between them: 100 + 100 + 50 proves the worker drains runnable
    // backlog instead of sleeping after the first batch.
    let (first, _) = Daemon::drive_projection_batch(
        &repo,
        &table,
        Box::new(FixedEmbedder { dim: 384 }),
        100,
        false,
    )
    .await;
    assert_eq!(first.unwrap(), 100);
    let (second, _) = Daemon::drive_projection_batch(
        &repo,
        &table,
        Box::new(FixedEmbedder { dim: 384 }),
        100,
        false,
    )
    .await;
    assert_eq!(second.unwrap(), 100);
    let (third, _) = Daemon::drive_projection_batch(
        &repo,
        &table,
        Box::new(FixedEmbedder { dim: 384 }),
        100,
        false,
    )
    .await;
    assert_eq!(third.unwrap(), 50);
    assert_eq!(repo.projection_lag().unwrap(), 0);
}

/// Lexical drive resolves text rows without vectors: the job ackes and
/// the row stays NULL-vectored (dense excludes NULLs explicitly).
#[tokio::test]
async fn lexical_drive_resolves_text_rows_without_vectors() {
    use ltmrs_domain::command::DomainCommand;
    use ltmrs_search::search::projector::Projector;
    use ltmrs_search::search::table::SearchTable;

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(
            dir.path().join("store").to_str().unwrap(),
            Arc::clone(&clock),
        )
        .unwrap(),
    );
    repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
    let table_dir = dir.path().join("table");
    std::fs::create_dir_all(&table_dir).unwrap();
    let table = SearchTable::open(table_dir.to_str().unwrap())
        .await
        .unwrap();
    let ctx = ltmrs_domain::command::CommandContext {
        store_generation: StoreGeneration::FIRST,
        frontend_id: fe(1),
        channel_id: ch(1),
        session: None,
        operation_id: OperationId::new(Uuid::from_u128(1)),
        request_digest: "lexical-drive".to_string(),
        deadline_millis: None,
        scope: Scope::default(),
        retry_epoch: 1,
    };
    repo.apply(
        &ctx,
        &DomainCommand::AddMemory {
            memory: test_memory(1),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let resolved = Projector::project_pending_lexical(&repo, &table, 10)
        .await
        .unwrap();
    assert_eq!(resolved, 1);
    assert!(!repo.has_pending_projection(test_memory(1).id).unwrap());
    assert_eq!(
        table.count_rows(Some("embedding IS NULL")).await.unwrap(),
        1,
        "lexical rows stay NULL-vectored"
    );
}

/// E5-upgrade backfill: memories projected while lexical-only (NULL
/// rows, jobs acked) are requeued for dense embedding.
#[tokio::test]
async fn backfill_requeues_lexical_null_vector_rows() {
    use ltmrs_domain::command::DomainCommand;
    use ltmrs_search::search::projector::Projector;
    use ltmrs_search::search::table::SearchTable;

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(
            dir.path().join("store").to_str().unwrap(),
            Arc::clone(&clock),
        )
        .unwrap(),
    );
    repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
    let table_dir = dir.path().join("table");
    std::fs::create_dir_all(&table_dir).unwrap();
    let table = SearchTable::open(table_dir.to_str().unwrap())
        .await
        .unwrap();
    let ctx = ltmrs_domain::command::CommandContext {
        store_generation: StoreGeneration::FIRST,
        frontend_id: fe(1),
        channel_id: ch(1),
        session: None,
        operation_id: OperationId::new(Uuid::from_u128(1)),
        request_digest: "lexical-backfill".to_string(),
        deadline_millis: None,
        scope: Scope::default(),
        retry_epoch: 1,
    };
    repo.apply(
        &ctx,
        &DomainCommand::AddMemory {
            memory: test_memory(1),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    Projector::project_pending_lexical(&repo, &table, 10)
        .await
        .unwrap();
    assert!(!repo.has_pending_projection(test_memory(1).id).unwrap());
    Daemon::backfill_null_vectors(&repo, &table).await;
    assert!(
        repo.has_pending_projection(test_memory(1).id).unwrap(),
        "NULL-vector rows must be requeued for dense embedding"
    );
}

/// Health serves the live generation (not a hardcoded FIRST): after a
/// generation switch the report follows the store.
#[tokio::test]
async fn health_reports_live_generation_not_first() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::resolve(dir.path(), "health-store");
    let config = DaemonConfig {
        store_path: dir.path().join("store").to_str().unwrap().to_string(),
        ..Default::default()
    };
    let daemon = Daemon::start(&paths, config).await.unwrap();
    let report = daemon.health_report(true, true);
    assert_eq!(report.store_generation, StoreGeneration::FIRST);
    assert!(report.ready);
    daemon
        .dispatcher_arc()
        .repo_arc()
        .set_store_generation(StoreGeneration::new(2))
        .unwrap();
    let report = daemon.health_report(true, true);
    assert_eq!(report.store_generation.as_u64(), 2);
    assert!(report.ready);
}
