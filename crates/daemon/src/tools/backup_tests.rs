//! backup tool tests (moved verbatim from `tools.rs`).

use super::execute_tool;
use super::test_support::*;
use crate::envelope::IpcEnvelope;
use ltmrs_compat::lemma::tool_args::{
    BackupCreateArgs, BackupPreviewArgs, BackupRestoreArgs, MemoryAddArgs, SessionStartArgs,
    ToolArgs,
};
use ltmrs_domain::id::OperationId;
use uuid::Uuid;

/// backup_create backs up through the tool surface and verifies the
/// archive; a missing directory fails explicitly (never invented).
/// backup_preview reports readiness with a token; missing path fails.
#[test]
fn backup_preview_reports_ready_with_token() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(&disp, 1, "## Preview Me\n\n### Context\nPreview fixture.");
    let out = tempfile::tempdir().unwrap();
    let create = ToolArgs::BackupCreate(BackupCreateArgs {
        directory: Some(out.path().to_str().unwrap().to_string()),
    });
    let result = run(&disp, &tool_call(2, create.clone()), &create);
    assert!(!result_is_error(&result));
    let path = result_structured(&result).unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
        path: Some(path.clone()),
    });
    let result = run(&disp, &tool_call(3, preview.clone()), &preview);
    assert!(!result_is_error(&result));
    let text = result_text(&result);
    assert!(text.contains("READY"), "got: {text}");
    let structured = result_structured(&result).unwrap();
    assert_eq!(structured["readiness"]["status"], "ready");
    let token = structured["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!token.is_empty());
    assert!(structured["expires_at"].as_u64().unwrap() > 0);

    // Missing path fails explicitly.
    let missing = ToolArgs::BackupPreview(BackupPreviewArgs { path: None });
    let result = run(&disp, &tool_call(4, missing.clone()), &missing);
    assert!(result_is_error(&result));
}

/// Dead channels must not block restore: after two sessions bound two
/// channels (prior runs leave persisted bindings behind), preview with
/// no LIVE connection still reports READY — readiness counts live
/// connections, not registry history.
#[test]
fn backup_preview_ignores_dead_channels() {
    let (disp, _dir) = test_dispatcher();
    disp.repo().issue_namespace(fe(2), ch(2), 1000).unwrap();
    add_fragment(&disp, 1, "## Preview Me\n\n### Context\nPreview fixture.");
    for (n, op) in [(1u64, 10u64), (2, 11)] {
        let start = ToolArgs::SessionStart(SessionStartArgs {
            task_type: "debugging".to_string(),
            technologies: vec![],
            initial_approach: None,
        });
        let env = IpcEnvelope {
            frontend_id: fe(n),
            channel_id: ch(n),
            operation_id: OperationId::new(Uuid::from_u128(op as u128)),
            ..tool_call(op, start.clone())
        };
        run(&disp, &env, &start);
    }
    assert_eq!(disp.registry().channel_count(), 2);
    let out = tempfile::tempdir().unwrap();
    let create = ToolArgs::BackupCreate(BackupCreateArgs {
        directory: Some(out.path().to_str().unwrap().to_string()),
    });
    let result = run(&disp, &tool_call(2, create.clone()), &create);
    assert!(!result_is_error(&result));
    let path = result_structured(&result).unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
        path: Some(path.clone()),
    });
    let result = run(&disp, &tool_call(3, preview.clone()), &preview);
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert_eq!(
        structured["readiness"]["status"], "ready",
        "dead channels must not block restore, got: {structured:?}"
    );
    assert!(
        structured["confirmation_token"].as_str().is_some(),
        "ready preview must issue a token"
    );
}

/// Loss accounting through the tool surface: an evolved backup carrying
/// a future top-level snapshot key reports the unknown count at preview
/// (before the destructive step) and again in the restore report.
#[test]
fn backup_preview_and_restore_report_unknown_keys() {
    use ltmrs_domain::export::CanonicalExport;
    let (disp, _dir) = test_dispatcher();
    add_fragment(&disp, 1, "## Evolve Me\n\n### Context\nLoss fixture.");
    let out = tempfile::tempdir().unwrap();
    let create = ToolArgs::BackupCreate(BackupCreateArgs {
        directory: Some(out.path().to_str().unwrap().to_string()),
    });
    let result = run(&disp, &tool_call(2, create.clone()), &create);
    assert!(!result_is_error(&result));
    let path = result_structured(&result).unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    // Future-producer simulation: extra snapshot key, manifest re-signed.
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    v["snapshot"]["future_collection"] = serde_json::json!([{"kept": true}]);
    let evolved_snap: CanonicalExport = serde_json::from_value(v["snapshot"].clone()).unwrap();
    v["manifest"]["digest"] = serde_json::Value::String(evolved_snap.digest());
    let evolved = out.path().join("evolved.ltmrs-backup");
    std::fs::write(&evolved, serde_json::to_vec(&v).unwrap()).unwrap();

    let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
        path: Some(evolved.to_str().unwrap().to_string()),
    });
    let result = run(&disp, &tool_call(3, preview.clone()), &preview);
    assert!(
        !result_is_error(&result),
        "preview failed: {}",
        result_text(&result)
    );
    let text = result_text(&result);
    assert!(text.contains("1 unknown"), "got: {text}");
    let structured = result_structured(&result).unwrap();
    assert_eq!(
        structured["unknown_top_level"].as_u64(),
        Some(1),
        "preview must surface the count"
    );
    let token = structured["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string();

    let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
        confirmation_token: Some(token),
        confirm: Some(true),
    });
    let result = run(&disp, &tool_call(4, restore.clone()), &restore);
    assert!(
        !result_is_error(&result),
        "restore failed: {}",
        result_text(&result)
    );
    let text = result_text(&result);
    assert!(text.contains("1 unknown"), "got: {text}");
    assert_eq!(
        result_structured(&result).unwrap()["unknown_top_level"].as_u64(),
        Some(1),
        "restore report must surface the count"
    );
}

/// P1 (restore quiescence): writes acknowledged while a restore runs
/// must be either in the safety backup or in the post-replace store —
/// never ACKed and subsequently drained unseen. A hammer thread writes
/// continuously across the restore; the exclusive fence serializes it
/// outside the safety→replace window.
#[test]
fn backup_restore_never_loses_acknowledged_writes() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    let (disp, _dir) = test_dispatcher();
    let disp = Arc::new(disp);
    add_fragment(&disp, 1, "## Restore Base\n\n### Context\nQuiescence seed.");
    // Fat store: a wide safety→replace window so the hammer lands
    // mid-window writes deterministically (a tiny store would let
    // the restore slip between two hammer iterations).
    for n in 2..=200u64 {
        add_fragment(
            &disp,
            n,
            &format!("## Fatten {n}\n\n### Context\nWindow-widening xorblat{n}."),
        );
    }
    let out = tempfile::tempdir().unwrap();
    let create = ToolArgs::BackupCreate(BackupCreateArgs {
        directory: Some(out.path().to_str().unwrap().to_string()),
    });
    let result = run(&disp, &tool_call(2, create.clone()), &create);
    assert!(!result_is_error(&result));
    let backup_path = result_structured(&result).unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
        path: Some(backup_path.clone()),
    });
    let result = run(&disp, &tool_call(3, preview.clone()), &preview);
    assert!(!result_is_error(&result));
    let token = result_structured(&result).unwrap()["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string();
    // Hammer: distinct fragments until the restore returns (unbounded:
    // the restore call bounds the loop, so mid-window writes are
    // guaranteed, not timing luck). Dedup may reject near-identical
    // ones; only ACKed writes count.
    let stop = Arc::new(AtomicBool::new(false));
    let next_op = Arc::new(AtomicU64::new(100));
    let acked: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hammer = {
        let (disp, stop, next_op, acked) = (
            Arc::clone(&disp),
            Arc::clone(&stop),
            Arc::clone(&next_op),
            Arc::clone(&acked),
        );
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::SeqCst) && i < 200000 {
                i += 1;
                let op = next_op.fetch_add(1, Ordering::SeqCst);
                // Mostly-unique token sets per write (Jaccard on
                // whitespace tokens): shared words stay far below the
                // 0.80 dedup threshold so hammer writes acknowledge.
                let nonce: Vec<String> = (0..6).map(|k| format!("xorblat{i}x{k}")).collect();
                let fragment = format!(
                    "## Hammer {i} {op}\n\n### Context\nQuiescence probe {}.",
                    nonce.join(" ")
                );
                let args = ToolArgs::MemoryAdd(MemoryAddArgs {
                    fragment: fragment.clone(),
                    ..Default::default()
                });
                let env = tool_call(op, args.clone());
                let ok = match execute_tool(&disp, &env, &args) {
                    Ok(result) => !result_is_error(&result),
                    Err(_) => false,
                };
                if ok {
                    acked.lock().unwrap().push(fragment);
                }
            }
        })
    };
    let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
        confirmation_token: Some(token),
        confirm: Some(true),
    });
    // Warm up: only start the restore once the hammer is actively
    // acknowledging, so mid-window overlap is structural, not luck.
    while acked.lock().unwrap().len() < 5 {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let result = run(&disp, &tool_call(4, restore.clone()), &restore);
    stop.store(true, Ordering::SeqCst);
    hammer.join().unwrap();
    assert!(
        !result_is_error(&result),
        "restore failed: {}",
        result_text(&result)
    );
    let safety = result_structured(&result).unwrap()["safety_backup"]
        .as_str()
        .unwrap()
        .to_string();
    let acked = acked.lock().unwrap().clone();
    assert!(
        !acked.is_empty(),
        "hammer must acknowledge writes for the test to mean anything"
    );
    // Every ACKed hammer write is either pre-safety (in the safety
    // backup) or post-replace (in the live store). The fenced window
    // admits no third outcome.
    let safety_snap = ltmrs_interchange::backup::verify_backup_file(
        std::path::Path::new(&safety),
        ltmrs_interchange::backup::MAX_BACKUP_BYTES,
    )
    .unwrap()
    .snapshot;
    let live = disp.repo().export_snapshot().unwrap();
    let mut missing = Vec::new();
    for fragment in &acked {
        let in_safety = safety_snap.memories.iter().any(|m| &m.fragment == fragment);
        let in_live = live.memories.iter().any(|m| &m.fragment == fragment);
        if !in_safety && !in_live {
            missing.push(fragment.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "acknowledged writes lost across restore: {missing:?} ({} hammered)",
        acked.len()
    );
}

/// Full restore cycle with rollback through the safety file: alpha live,
/// backup alpha, add beta, restore (beta gone), restore safety (beta back).
/// Generation advances on every restore; sessions restore from the backup.
#[test]
fn backup_restore_end_to_end_with_rollback() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(
        &disp,
        1,
        "## Restore Alpha\n\n### Context\nPre-restore content.",
    );
    let out = tempfile::tempdir().unwrap();
    let create = ToolArgs::BackupCreate(BackupCreateArgs {
        directory: Some(out.path().to_str().unwrap().to_string()),
    });
    let result = run(&disp, &tool_call(2, create.clone()), &create);
    let backup_path = result_structured(&result).unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    let gen_before = disp.repo().store_generation().unwrap().as_u64();
    add_fragment(
        &disp,
        3,
        "## Restore Beta\n\n### Context\nPost-backup content.",
    );

    // Preview + restore the backup (beta disappears).
    let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
        path: Some(backup_path.clone()),
    });
    let result = run(&disp, &tool_call(4, preview.clone()), &preview);
    let token = result_structured(&result).unwrap()["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string();
    // A write lands between preview and confirm (same channel): the
    // replace drains it, so the report must acknowledge the delta
    // instead of dropping it silently.
    add_fragment(
        &disp,
        6,
        "## Restore Gamma\n\n### Context\nMid-window content.",
    );
    let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
        confirmation_token: Some(token),
        confirm: Some(true),
    });
    let result = run(&disp, &tool_call(5, restore.clone()), &restore);
    assert!(
        !result_is_error(&result),
        "restore failed: {}",
        result_text(&result)
    );
    let text = result_text(&result);
    assert!(text.contains("Restored 1 memories"), "got: {text}");
    assert!(text.contains("safety backup at"), "got: {text}");
    let structured = result_structured(&result).unwrap();
    // Gamma's add plus its topical auto-link both executed mid-window:
    // every executed write counts, each would have been drained.
    assert!(
        structured["live_writes_since_preview"].as_u64().unwrap() >= 1,
        "mid-window writes must be acknowledged, got: {structured:?}"
    );
    assert!(
        text.contains("live write(s) landed after the preview"),
        "report must name the delta, got: {text}"
    );
    let safety = structured["safety_backup"].as_str().unwrap().to_string();
    assert!(
        std::path::Path::new(&safety).exists(),
        "safety file published"
    );
    assert_eq!(
        disp.repo().store_generation().unwrap().as_u64(),
        gen_before + 1
    );
    let titles: Vec<String> = disp
        .repo()
        .export_full()
        .unwrap()
        .memories
        .iter()
        .map(|m| m.title.clone())
        .collect();
    assert!(
        !titles.iter().any(|t| t.contains("Beta")),
        "got: {titles:?}"
    );

    // Rollback: preview + restore the safety file (beta returns).
    // The first restore drained all namespaces: like a production
    // frontend after a generation cut, re-handshake (fresh namespace;
    // the drained epoch counter restarts at 1) before continuing.
    disp.repo().issue_namespace(fe(1), ch(1), 1000).unwrap();
    let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
        path: Some(safety.clone()),
    });
    let result = run(&disp, &tool_call(6, preview.clone()), &preview);
    assert!(!result_is_error(&result));
    let token = result_structured(&result).unwrap()["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string();
    let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
        confirmation_token: Some(token),
        confirm: Some(true),
    });
    let result = run(&disp, &tool_call(7, restore.clone()), &restore);
    assert!(
        !result_is_error(&result),
        "rollback failed: {}",
        result_text(&result)
    );
    assert_eq!(
        disp.repo().store_generation().unwrap().as_u64(),
        gen_before + 2
    );
    let titles: Vec<String> = disp
        .repo()
        .export_full()
        .unwrap()
        .memories
        .iter()
        .map(|m| m.title.clone())
        .collect();
    assert!(titles.iter().any(|t| t.contains("Beta")), "got: {titles:?}");
}

/// Restore demands an unused token plus explicit confirmation; a
/// refused confirm does not burn the token.
#[test]
fn backup_restore_requires_confirmation() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(&disp, 1, "## Confirm Me\n\n### Context\nConfirm fixture.");
    let out = tempfile::tempdir().unwrap();
    let create = ToolArgs::BackupCreate(BackupCreateArgs {
        directory: Some(out.path().to_str().unwrap().to_string()),
    });
    let result = run(&disp, &tool_call(2, create.clone()), &create);
    let path = result_structured(&result).unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    let preview = ToolArgs::BackupPreview(BackupPreviewArgs { path: Some(path) });
    let result = run(&disp, &tool_call(3, preview.clone()), &preview);
    let token = result_structured(&result).unwrap()["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string();

    // Unknown token rejected.
    let bad = ToolArgs::BackupRestore(BackupRestoreArgs {
        confirmation_token: Some("nope".to_string()),
        confirm: Some(true),
    });
    let result = run(&disp, &tool_call(4, bad.clone()), &bad);
    assert!(result_is_error(&result));

    // Missing confirmation explains REPLACE without consuming the token.
    let unconfirmed = ToolArgs::BackupRestore(BackupRestoreArgs {
        confirmation_token: Some(token.clone()),
        confirm: None,
    });
    let result = run(&disp, &tool_call(5, unconfirmed.clone()), &unconfirmed);
    assert!(result_is_error(&result));
    assert!(result_text(&result).contains("confirm=true"));

    // The same token still works after the refused confirm.
    let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
        confirmation_token: Some(token),
        confirm: Some(true),
    });
    let result = run(&disp, &tool_call(6, restore.clone()), &restore);
    assert!(!result_is_error(&result), "got: {}", result_text(&result));
}

#[test]
fn backup_create_tool_backs_up_and_verifies() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(&disp, 1, "## Backup One\n\n### Context\nFirst.");
    add_fragment(&disp, 2, "## Backup Two\n\n### Context\nSecond.");
    let out = tempfile::tempdir().unwrap();
    let args = ToolArgs::BackupCreate(BackupCreateArgs {
        directory: Some(out.path().to_str().unwrap().to_string()),
    });
    let env = tool_call(3, args.clone());
    let result = run(&disp, &env, &args);
    assert!(!result_is_error(&result));
    let text = result_text(&result);
    assert!(text.contains("Backed up 2 memories"), "got: {text}");
    assert!(text.contains("Digest: "), "got: {text}");
    let structured = result_structured(&result).unwrap();
    let path = structured["path"].as_str().unwrap().to_string();
    assert!(path.ends_with(".ltmrs-backup"), "got: {path}");
    assert!(std::path::Path::new(&path).exists());
    // Re-verify the produced file through the library boundary.
    let verified = ltmrs_interchange::backup::verify_backup_file(
        std::path::Path::new(&path),
        ltmrs_interchange::backup::MAX_BACKUP_BYTES,
    )
    .unwrap();
    assert_eq!(verified.counts["memories"], 2);

    // Missing directory fails explicitly.
    let missing = ToolArgs::BackupCreate(BackupCreateArgs { directory: None });
    let env = tool_call(4, missing.clone());
    let result = run(&disp, &env, &missing);
    assert!(result_is_error(&result));
    assert!(
        result_text(&result).contains("requires"),
        "got: {}",
        result_text(&result)
    );
}
/// P2-A: a generation cut drops runtime channel bindings. After a
/// restore, no pre-restore channel→session route may survive (the next
/// call on each channel binds fresh).
#[test]
fn restore_clears_channel_bindings() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    assert!(
        disp.registry().channel_session(fe(1), ch(1)).is_some(),
        "channel must be bound after start"
    );
    add_fragment(&disp, 2, "## Restore Me\n\n### Context\nBinding fixture.");
    let out = tempfile::tempdir().unwrap();
    let create = ToolArgs::BackupCreate(BackupCreateArgs {
        directory: Some(out.path().to_str().unwrap().to_string()),
    });
    let result = run(&disp, &tool_call(3, create.clone()), &create);
    let path = result_structured(&result).unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    let preview = ToolArgs::BackupPreview(BackupPreviewArgs { path: Some(path) });
    let result = run(&disp, &tool_call(4, preview.clone()), &preview);
    let token = result_structured(&result).unwrap()["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string();
    let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
        confirmation_token: Some(token),
        confirm: Some(true),
    });
    let result = run(&disp, &tool_call(5, restore.clone()), &restore);
    assert!(
        !result_is_error(&result),
        "restore failed: {}",
        result_text(&result)
    );
    assert!(
        disp.registry().channel_session(fe(1), ch(1)).is_none(),
        "pre-restore binding must not survive the generation cut"
    );
}

/// Clearing bindings on restore must also persist, or a crash before
/// the next persist reloads the pre-restore sessions file whose routes
/// point at drained sessions.
#[test]
fn restore_persists_cleared_bindings() {
    let (disp, dir) = test_dispatcher();
    let sessions_file = dir.path().join("sessions.json");
    disp.set_sessions_path(Some(sessions_file.clone()));
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    disp.persist_sessions().unwrap();
    let (reloaded, _) = crate::registry::FrontendRegistry::load(&sessions_file).unwrap();
    assert!(
        reloaded.channel_session(fe(1), ch(1)).is_some(),
        "baseline file must carry the binding"
    );
    add_fragment(&disp, 2, "## Restore Me\n\n### Context\nBinding fixture.");
    let out = tempfile::tempdir().unwrap();
    let create = ToolArgs::BackupCreate(BackupCreateArgs {
        directory: Some(out.path().to_str().unwrap().to_string()),
    });
    let result = run(&disp, &tool_call(3, create.clone()), &create);
    let path = result_structured(&result).unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    let preview = ToolArgs::BackupPreview(BackupPreviewArgs { path: Some(path) });
    let result = run(&disp, &tool_call(4, preview.clone()), &preview);
    let token = result_structured(&result).unwrap()["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string();
    let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
        confirmation_token: Some(token),
        confirm: Some(true),
    });
    let result = run(&disp, &tool_call(5, restore.clone()), &restore);
    assert!(
        !result_is_error(&result),
        "restore failed: {}",
        result_text(&result)
    );
    let (reloaded, _) = crate::registry::FrontendRegistry::load(&sessions_file).unwrap();
    assert!(
        reloaded.channel_session(fe(1), ch(1)).is_none(),
        "reloaded file must not point at drained sessions"
    );
}
