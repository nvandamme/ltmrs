//! Generation-lifecycle tests (moved verbatim from `repository.rs`).

use super::CanonicalRepository;
use super::test_support::*;
use ltmrs_domain::command::{DomainCommand, DomainErrorCode};
use ltmrs_domain::id::{ModelFingerprint, StoreGeneration};
use ltmrs_domain::projection::GenerationStatus;

/// Blue-green cutover is atomic: staging and partial builds never move the
/// active pointer; one activation publishes the new generation in a single
/// step while the old rows stay retained for rollback.
#[test]
fn cutover_is_atomic_and_requires_readiness() {
    let (repo, _dir) = repo_with_ns();
    for n in [1u64, 2] {
        repo.apply(
            &ctx(n, &format!("m{n}")),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), &format!("m{n}")),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    assert_eq!(next, StoreGeneration::new(2));
    // Staged only: pointer unchanged, activation refused.
    assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
    let rec = repo.generation_record(next).unwrap().unwrap();
    assert_eq!(rec.status, GenerationStatus::Staged);
    assert_eq!(rec.desired_memories, 2);
    // Partial build: still not ready.
    repo.note_generation_progress(next, 1).unwrap();
    assert_eq!(
        repo.generation_record(next).unwrap().unwrap().status,
        GenerationStatus::Building
    );
    assert!(repo.activate_generation(next).is_err());
    assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
    // Complete build: ready, then exactly-one-step activation.
    repo.note_generation_progress(next, 2).unwrap();
    assert_eq!(
        repo.generation_record(next).unwrap().unwrap().status,
        GenerationStatus::Ready
    );
    repo.activate_generation(next).unwrap();
    assert_eq!(repo.store_generation().unwrap(), next);
    assert_eq!(
        repo.generation_record(next).unwrap().unwrap().status,
        GenerationStatus::Active
    );
    // Previous generation retired with a timestamp for the reaper.
    let prev = repo
        .generation_record(StoreGeneration::FIRST)
        .unwrap()
        .unwrap();
    assert_eq!(prev.status, GenerationStatus::Retired);
}

/// The watermark is conservative: memories added mid-build make the staged
/// denominator stale, and activation must refuse until a fresh build
/// covers them (no silent partial generation).
#[test]
fn activate_rejects_stale_watermark() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    // A concurrent write lands mid-build.
    repo.apply(
        &ctx(2, "m2"),
        &DomainCommand::AddMemory {
            memory: memory(eid(2), "m2"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    // The build covered only the staged denominator: Ready by count, but
    // activation sees the newer recallable memory and refuses.
    repo.note_generation_progress(next, 1).unwrap();
    assert_eq!(
        repo.generation_record(next).unwrap().unwrap().status,
        GenerationStatus::Ready
    );
    assert!(repo.activate_generation(next).is_err());
    assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
}

/// Rollback needs no rebuild: a retired generation's rows are retained,
/// so re-activating it flips the pointer back in one step.
#[test]
fn rollback_restores_previous_generation() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    repo.note_generation_progress(next, 1).unwrap();
    repo.activate_generation(next).unwrap();
    assert_eq!(repo.store_generation().unwrap(), next);
    // Roll back: no build, just re-activation.
    repo.activate_generation(StoreGeneration::FIRST).unwrap();
    assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
    assert_eq!(
        repo.generation_record(StoreGeneration::FIRST)
            .unwrap()
            .unwrap()
            .status,
        GenerationStatus::Active
    );
    assert_eq!(
        repo.generation_record(next).unwrap().unwrap().status,
        GenerationStatus::Retired
    );
}

/// A canonical write during an open build dirties the pipeline: activation
/// is refused until a fresh build is reported, so a mid-build write can
/// never slip into a silently partial generation.
#[test]
fn mid_build_write_blocks_activation_until_renote() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    repo.note_generation_progress(next, 1).unwrap();
    // A concurrent write lands mid-build.
    repo.apply(
        &ctx(2, "m2"),
        &DomainCommand::AddMemory {
            memory: memory(eid(2), "m2"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let err = repo.activate_generation(next).unwrap_err();
    assert!(
        err.message.contains("dirty"),
        "dirty pipeline must refuse activation, got: {}",
        err.message
    );
    assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
    // Fresh build covering both memories converges the cutover.
    repo.note_generation_progress(next, 2).unwrap();
    repo.activate_generation(next).unwrap();
    assert_eq!(repo.store_generation().unwrap(), next);
}

/// Deletions dirty the pipeline too: a forget removes projected content,
/// so the build must be refreshed before activation.
#[test]
fn forget_mid_build_dirties_pipeline() {
    let (repo, _dir) = repo_with_ns();
    for n in [1u64, 2] {
        repo.apply(
            &ctx(n, &format!("m{n}")),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), &format!("m{n}")),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    repo.note_generation_progress(next, 2).unwrap();
    repo.apply(
        &ctx(3, "f1"),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ltmrs_domain::command::ForgetMode::Delete,
        },
    )
    .unwrap();
    assert!(repo.activate_generation(next).is_err());
}

/// Writes with no open pipeline touch no records: staging starts clean.
#[test]
fn writes_without_pipeline_leave_no_dirty_state() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    let rec = repo.generation_record(next).unwrap().unwrap();
    assert!(!rec.build_dirty);
}

/// Pre-upgrade records without the flag decode as dirty (fail-closed):
/// an open pipeline of unknown build state must be re-reported, never
/// trusted clean.
#[test]
fn old_record_without_flag_decodes_dirty() {
    let raw = r#"{"generation":2,"model_fingerprint":7,"status":"Staged","desired_memories":1,"projected_memories":0,"updated_at_millis":1000}"#;
    let rec: ltmrs_domain::projection::GenerationRecord = serde_json::from_str(raw).unwrap();
    assert!(rec.build_dirty);
}

/// A dirty retired pipeline still rolls back: retained rows need no
/// build, so the dirty gate (like the watermark) exempts rollback.
#[test]
fn dirty_retired_generation_still_rolls_back() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    repo.note_generation_progress(next, 1).unwrap();
    // Mid-build write, then abandon instead of rebuilding.
    repo.apply(
        &ctx(2, "m2"),
        &DomainCommand::AddMemory {
            memory: memory(eid(2), "m2"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    repo.abandon_generation(next).unwrap();
    // Rollback to the abandoned generation succeeds despite the dirt:
    // it reuses retained rows.
    repo.activate_generation(next).unwrap();
    assert_eq!(repo.store_generation().unwrap(), next);
}

/// Abandoning a staged pipeline retires it (partial rows become reaper
/// food) and unblocks fresh staging. Active generations cannot be
/// abandoned — restore or cut over instead.
#[test]
fn abandon_releases_staged_pipeline() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let staged = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    repo.abandon_generation(staged).unwrap();
    assert_eq!(
        repo.generation_record(staged).unwrap().unwrap().status,
        GenerationStatus::Retired
    );
    // Fresh pipeline stages immediately after (numbers keep advancing).
    let next = repo.stage_generation(ModelFingerprint::new(8)).unwrap();
    assert_eq!(next, StoreGeneration::new(3));
    // Unknown and active generations cannot be abandoned.
    assert!(repo.abandon_generation(StoreGeneration::new(99)).is_err());
    assert!(repo.abandon_generation(StoreGeneration::FIRST).is_err());
}

/// A restore retires every live pipeline record: pre-restore staged
/// workers lose publish rights and fresh staging works immediately.
#[test]
fn restore_retires_staged_pipeline() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let staged = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    repo.set_store_generation(StoreGeneration::new(9)).unwrap();
    assert_eq!(repo.store_generation().unwrap(), StoreGeneration::new(9));
    assert_eq!(
        repo.generation_record(staged).unwrap().unwrap().status,
        GenerationStatus::Retired
    );
    let next = repo.stage_generation(ModelFingerprint::new(8)).unwrap();
    assert_eq!(next, StoreGeneration::new(10));
}

/// Re-activating the active generation succeeds, so operator retries
/// after a timeout do not look like failures.
#[test]
fn activate_is_idempotent() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    repo.note_generation_progress(next, 1).unwrap();
    repo.activate_generation(next).unwrap();
    repo.activate_generation(next).unwrap();
    assert_eq!(repo.store_generation().unwrap(), next);
}

/// An interrupted build (staged record, no activation) still resolves to
/// the last verified generation after reopen — never a partial one.
#[test]
fn interrupted_build_resolves_to_last_active() {
    let (repo, dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let staged = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    assert_eq!(staged, StoreGeneration::new(2));
    // Simulate a crash between stage and activate: drop the handle and
    // reopen over the same directory (dir stays alive, like the Fjall
    // kill/reopen durability test).
    let path = dir.path().to_str().unwrap().to_string();
    drop(repo);
    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo = CanonicalRepository::open_with_clock(&path, clock).unwrap();
    assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
    let staged = repo
        .generation_record(StoreGeneration::new(2))
        .unwrap()
        .unwrap();
    assert_eq!(staged.status, GenerationStatus::Staged);
    // A stale activation attempt against the partial build still fails.
    assert!(repo.activate_generation(StoreGeneration::new(2)).is_err());
}
#[test]
fn apply_with_retired_generation_fails_closed() {
    let (repo, _dir) = repo_with_ns();
    let mut c = ctx(1, "d1");
    c.store_generation = StoreGeneration::new(2);
    let err = repo
        .apply(
            &c,
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "x"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::StaleGeneration);
}
