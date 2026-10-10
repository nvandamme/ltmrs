//! Suggestion/feedback tests (moved verbatim from `repository.rs`).

use super::SessionLinkField;
use super::test_support::*;
use ltmrs_domain::command::DomainCommand;
use uuid::Uuid;

#[test]
fn feedback_updates_counters_and_records_event() {
    let (repo, _dir) = repo_with_ns();
    let m = memory(eid(1), "hello");
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: m,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();

    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::Feedback {
            memory_id: eid(1),
            useful: true,
        },
    )
    .unwrap();

    // Domain state: observable counters updated.
    let rec = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    assert_eq!(rec.positive_feedback, 1);
    assert!(rec.confidence > 0.5);

    // Diagnostic telemetry: a separate event log records the feedback.
    let events = repo.feedback_events().unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].useful);
    assert_eq!(events[0].memory_id, eid(1));
}

#[test]
fn feedback_replay_does_not_double_record() {
    let (repo, _dir) = repo_with_ns();
    let m = memory(eid(1), "hello");
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: m,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();

    let c = ctx(2, "d2");
    repo.apply(
        &c,
        &DomainCommand::Feedback {
            memory_id: eid(1),
            useful: true,
        },
    )
    .unwrap();
    // Replay the same operation (same key + digest).
    repo.apply(
        &c,
        &DomainCommand::Feedback {
            memory_id: eid(1),
            useful: true,
        },
    )
    .unwrap();

    // One logical feedback produces one event, not two.
    let events = repo.feedback_events().unwrap();
    assert_eq!(events.len(), 1, "replay must not double-record the event");
    let rec = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    assert_eq!(rec.positive_feedback, 1);
}
/// P1/P2 session_end exactly-once: improvement suggestions are filed
/// inside the end transaction, so concurrent duplicate deliveries of
/// the same end operation yield exactly one Suggestion per line —
/// never one per delivery (file_suggestion's atomic IDs alone only
/// prevent overwrites, not duplicates).
#[test]
fn session_end_files_suggestions_exactly_once_per_operation() {
    use ltmrs_domain::guide::Guide;
    use ltmrs_domain::id::SessionHandle;
    use ltmrs_domain::memory::Instant;
    use ltmrs_domain::session::{SessionOp, TaskOutcome};
    use std::sync::{Arc, Barrier};
    let (repo, _dir) = repo_with_ns();
    // Guide with a failing record (total 3, rate 0.00): a Failure end
    // using it yields exactly one improvement line.
    repo.put_guide(&Guide {
        name: "git".into(),
        category: "dev-tool".into(),
        description: String::new(),
        contexts: vec![],
        learnings: vec![],
        usage_count: 0,
        last_used: None,
        success_count: 0,
        failure_count: 3,
        anti_patterns: vec![],
        pitfalls: vec![],
        depends_on: vec![],
        enables: vec![],
        source_memories: vec![],
        validated_by: vec![],
        superseded_by: None,
        deprecated: false,
        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
        created_at: Instant::new(0),
        updated_at: Instant::new(0),
    })
    .unwrap();
    let handle = SessionHandle::new(Uuid::from_u128(700));
    match repo
        .session_start_tx(
            &scope(700, "digest-start"),
            handle,
            None,
            None,
            vec![],
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, handle),
        other => panic!("expected Applied, got {other:?}"),
    }
    repo.track_session_link(
        &admit(&repo, 701, "track"),
        handle,
        SessionLinkField::GuideUsed,
        &["git".to_string()],
    )
    .unwrap();
    // Eight duplicate deliveries of the same end operation, released
    // together: exactly one Applies, the rest Replay.
    let repo = Arc::new(repo);
    let start = Arc::new(Barrier::new(9));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let repo = Arc::clone(&repo);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            start.wait();
            repo.session_end_tx(
                &scope(702, "digest-end"),
                handle,
                TaskOutcome::Failure,
                None,
                vec![],
                1000,
            )
            .unwrap()
        }));
    }
    start.wait();
    let mut applied = 0;
    for h in handles {
        match h.join().unwrap() {
            SessionOp::Applied((_, lines, true)) => {
                applied += 1;
                assert_eq!(lines.len(), 1, "one improvement line expected");
            }
            SessionOp::Replayed((_, lines, _)) => {
                assert_eq!(lines.len(), 1, "replay renders the same line");
            }
            other => panic!("unexpected end outcome: {other:?}"),
        }
    }
    assert_eq!(applied, 1, "exactly one delivery may apply");
    // Exactly one Suggestion for the line — never one per delivery.
    let suggestions = repo.get_suggestions().unwrap();
    assert_eq!(
        suggestions.len(),
        1,
        "exactly one suggestion per line, got {suggestions:?}"
    );
    assert_eq!(
        suggestions[0].session_id.as_deref(),
        Some(handle.as_uuid().to_string().as_str())
    );
}

/// Concurrent suggestion filing must yield distinct ids: max+1 computed
/// outside the insert transaction lets two writers claim the same id
/// and silently overwrite each other.
#[test]
fn concurrent_suggestion_filing_yields_distinct_ids() {
    use std::sync::{Arc, Barrier};
    let (repo, _dir) = repo_with_ns();
    let repo = Arc::new(repo);
    let start = Arc::new(Barrier::new(9));
    let mut handles = Vec::new();
    for t in 0..8 {
        let repo = Arc::clone(&repo);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            start.wait();
            let mut ids = Vec::new();
            for i in 0..50 {
                // Retry on conflicts (concurrent filing discipline);
                // duplicates are the bug, conflicts are not (namespace
                // issuance precedent).
                let mut filed = None;
                for _ in 0..20 {
                    match repo.file_suggestion(
                        Some(format!("session-{t}")),
                        format!("suggestion {t}-{i}"),
                        1000,
                    ) {
                        Ok(s) => {
                            filed = Some(s.id);
                            break;
                        }
                        Err(_) => continue,
                    }
                }
                ids.push(filed.expect("filing must succeed after retries"));
            }
            ids
        }));
    }
    start.wait();
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }
    assert_eq!(all.len(), 400, "every filing must succeed after retries");
    all.sort_unstable();
    let distinct: Vec<u64> = {
        let mut d = all.clone();
        d.dedup();
        d
    };
    assert_eq!(
        distinct.len(),
        400,
        "suggestion ids must be distinct (double-claim overwrites a record)"
    );
    assert_eq!(repo.get_suggestions().unwrap().len(), 400);
}
