//! C1 fence tests (Slice C): version matrix, WAL refusal, source-unchanged.

use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str, args: &[&str]) -> PathBuf {
    let out = std::env::temp_dir().join(format!("mc-c1-{name}.db"));
    let _ = std::fs::remove_file(&out);
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/build_fixture.py");
    let status = Command::new("python3")
        .arg(script)
        .arg(&out)
        .args(args)
        .status()
        .expect("python3 must run the fixture builder");
    assert!(status.success(), "fixture builder failed for {name}");
    out
}

/// Supported version opens and reports its version.
#[test]
fn supported_version_opens() {
    let path = fixture("supported", &[]);
    let (_dir, conn) = ltmrs_mc_import::open_readonly_copy(&path).unwrap();
    assert_eq!(ltmrs_mc_import::persisted_version(&conn).unwrap(), 95);
}

/// Newer-than-supported refuses fail-closed.
#[test]
fn newer_version_refuses() {
    let path = fixture("newer", &["--version=999"]);
    let err = ltmrs_mc_import::open_readonly_copy(&path).unwrap_err();
    assert!(
        err.to_string().contains("unsupported Magic Context schema v999"),
        "got: {err}"
    );
}

/// WAL-mode source refuses (checkpoint first).
#[test]
fn wal_mode_refuses() {
    let path = fixture("wal", &["--wal"]);
    let err = ltmrs_mc_import::open_readonly_copy(&path).unwrap_err();
    assert!(err.to_string().contains("WAL mode"), "got: {err}");
}

/// Source file is byte-identical after a successful open.
#[test]
fn source_unchanged_after_open() {
    use sha2::Digest;
    let path = fixture("unchanged", &[]);
    let before = sha2::Sha256::digest(std::fs::read(&path).unwrap());
    let (_dir, _conn) = ltmrs_mc_import::open_readonly_copy(&path).unwrap();
    let after = sha2::Sha256::digest(std::fs::read(&path).unwrap());
    assert_eq!(before[..], after[..]);
}

/// C2 RED: unknown harness refuses.
#[test]
fn unknown_harness_refuses() {
    let err = ltmrs_mc_import::scope::resolve_scope("nope", "/repo/shop").unwrap_err();
    assert!(err.to_string().contains("unknown harness"), "got: {err}");
}

/// C2 RED: `..` traversal refuses.
#[test]
fn parent_traversal_refuses() {
    let err = ltmrs_mc_import::scope::resolve_scope("opencode", "/repo/../etc").unwrap_err();
    assert!(err.to_string().contains("`..`"), "got: {err}");
}

/// C2 RED: non-repo path refuses.
#[test]
fn non_repo_path_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().to_str().unwrap().to_string();
    let err = ltmrs_mc_import::scope::resolve_scope("opencode", &p).unwrap_err();
    assert!(err.to_string().contains("not inside a git repo"), "got: {err}");
}

/// C2: same path string under different git roots lands in different scopes.
#[test]
fn same_path_different_roots_differ() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    for d in [&a, &b] {
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        std::fs::create_dir_all(d.path().join("proj")).unwrap();
    }
    // Same leaf name, different canonical roots.
    let pa = a.path().join("proj").to_str().unwrap().to_string();
    let pb = b.path().join("proj").to_str().unwrap().to_string();
    let sa = ltmrs_mc_import::scope::resolve_scope("opencode", &pa).unwrap();
    let sb = ltmrs_mc_import::scope::resolve_scope("opencode", &pb).unwrap();
    assert_ne!(sa.git_root, sb.git_root);
    // And identical inputs resolve identically (deterministic).
    let sa2 = ltmrs_mc_import::scope::resolve_scope("opencode", &pa).unwrap();
    assert_eq!(sa, sa2);
}

/// C3: active row maps with external alias + loss accounted.
#[test]
fn active_row_maps_with_loss() {
    use ltmrs_mc_import::map::{map_memory, McMemory};
    let row = McMemory {
        id: 7, project_path: "/repo/shop".into(), category: "ARCHITECTURE".into(),
        content: "DECISION: Use Fjall.".into(), normalized_hash: "abc".into(),
        importance: Some(80), scope: "project".into(), source_type: "historian".into(),
        seen_count: 3, retrieval_count: 1, first_seen_at: 1, created_at: 2,
        updated_at: 3, last_seen_at: 4, status: "active".into(),
    };
    let mapped = map_memory(&row, None).unwrap();
    assert_eq!(mapped.memory.external_alias.unwrap().as_str(), "mc:7");
    assert!(mapped.loss.iter().any(|l| l.field == "importance"));
    assert!(mapped.loss.iter().any(|l| l.field == "scope"));
}

/// C3: non-active rows refuse (never silently dropped — loud error).
#[test]
fn non_active_row_refuses() {
    use ltmrs_mc_import::map::{map_memory, McMemory};
    let row = McMemory {
        id: 8, project_path: "/repo/shop".into(), category: "X".into(),
        content: "gone".into(), normalized_hash: "x".into(), importance: None,
        scope: "".into(), source_type: "".into(), seen_count: 0,
        retrieval_count: 0, first_seen_at: 0, created_at: 0, updated_at: 0,
        last_seen_at: 0, status: "superseded".into(),
    };
    let err = map_memory(&row, None).unwrap_err();
    assert!(err.to_string().contains("only active rows import"), "got: {err}");
}

/// C3: mapped ids are deterministic and never collide with raw MC ids.
#[test]
fn mapped_ids_deterministic() {
    use ltmrs_mc_import::map::{map_memory, McMemory};
    let mk = || McMemory {
        id: 9, project_path: "/x".into(), category: "C".into(),
        content: "content here is long enough".into(), normalized_hash: "h".into(),
        importance: None, scope: "".into(), source_type: "".into(), seen_count: 0,
        retrieval_count: 0, first_seen_at: 0, created_at: 0, updated_at: 0,
        last_seen_at: 0, status: "active".into(),
    };
    let a = map_memory(&mk(), None).unwrap().memory.id;
    let b = map_memory(&mk(), None).unwrap().memory.id;
    assert_eq!(a, b);
}
