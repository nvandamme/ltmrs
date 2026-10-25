//! Fixture round-trip: a Lemma-format SQLite image built with rusqlite must
//! map into staging JSON shaped like the interchange snapshot, with exact
//! loss accounting.

use std::collections::BTreeMap;
use std::path::Path;

use ltmrs_lemma_import::import_to_json;

/// Minimal Lemma v8 DDL for the exercised tables (column definitions copied
/// from the pinned upstream `dist/db/schema.js`; FTS + cache tables included
/// to prove they are ignored/dropped, not mapped).
const FIXTURE_DDL: &str = r#"
CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at TEXT DEFAULT (datetime('now')) NOT NULL);
CREATE TABLE memories (
  id INTEGER PRIMARY KEY AUTOINCREMENT, legacy_id TEXT UNIQUE, title TEXT NOT NULL,
  fragment TEXT NOT NULL, description TEXT, type TEXT NOT NULL, project TEXT,
  source TEXT DEFAULT 'ai' NOT NULL, confidence REAL DEFAULT 0.5 NOT NULL,
  context_tags TEXT, access_count INTEGER DEFAULT 0 NOT NULL,
  last_accessed_at TEXT, negative_hits INTEGER DEFAULT 0 NOT NULL,
  positive_feedback INTEGER DEFAULT 0 NOT NULL, negative_feedback INTEGER DEFAULT 0 NOT NULL,
  quality_score REAL, refinement_count INTEGER DEFAULT 0 NOT NULL,
  parent_id INTEGER REFERENCES memories(id), session_id TEXT, task_type TEXT,
  distill_candidate INTEGER DEFAULT 0, related_guides TEXT, associated_with TEXT,
  last_refined TEXT, created_at TEXT DEFAULT (datetime('now')) NOT NULL,
  updated_at TEXT DEFAULT (datetime('now')) NOT NULL,
  access_window INTEGER DEFAULT 0 NOT NULL, invalidated_at TEXT);
CREATE TABLE relations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
  target_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
  type TEXT NOT NULL, note TEXT, created_at TEXT DEFAULT (datetime('now')) NOT NULL,
  UNIQUE(source_id, target_id, type));
CREATE TABLE guides (
  id INTEGER PRIMARY KEY AUTOINCREMENT, guide TEXT NOT NULL UNIQUE, category TEXT NOT NULL,
  description TEXT NOT NULL, mission TEXT, protocol TEXT, anti_patterns TEXT, pitfalls TEXT,
  deprecated INTEGER DEFAULT 0 NOT NULL, superseded_by TEXT, usage_count INTEGER DEFAULT 0 NOT NULL,
  success_count INTEGER DEFAULT 0 NOT NULL, failure_count INTEGER DEFAULT 0 NOT NULL,
  last_used_at TEXT, depends_on TEXT, enables TEXT, source_memories TEXT, validated_by TEXT,
  last_refined TEXT, created_at TEXT DEFAULT (datetime('now')) NOT NULL,
  updated_at TEXT DEFAULT (datetime('now')) NOT NULL);
CREATE TABLE guide_contexts (id INTEGER PRIMARY KEY AUTOINCREMENT,
  guide_id INTEGER NOT NULL REFERENCES guides(id) ON DELETE CASCADE, context TEXT NOT NULL,
  UNIQUE(guide_id, context));
CREATE TABLE guide_learnings (id INTEGER PRIMARY KEY AUTOINCREMENT,
  guide_id INTEGER NOT NULL REFERENCES guides(id) ON DELETE CASCADE, learning TEXT NOT NULL,
  UNIQUE(guide_id, learning));
CREATE TABLE guide_memory_links (guide_id INTEGER NOT NULL REFERENCES guides(id) ON DELETE CASCADE,
  memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE, link_type TEXT NOT NULL,
  PRIMARY KEY (guide_id, memory_id, link_type));
CREATE TABLE sessions (
  id TEXT PRIMARY KEY, task_type TEXT, technologies TEXT, initial_approach TEXT,
  final_approach TEXT, approach_changed INTEGER DEFAULT 0, outcome TEXT,
  refinement_attempts INTEGER DEFAULT 0, self_critique_count INTEGER DEFAULT 0,
  lessons TEXT, tool_calls INTEGER DEFAULT 0 NOT NULL, memory_count INTEGER DEFAULT 0 NOT NULL,
  status TEXT DEFAULT 'active', project TEXT,
  started_at TEXT DEFAULT (datetime('now')) NOT NULL, ended_at TEXT);
CREATE TABLE session_attempts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  approach TEXT NOT NULL, rationale TEXT, outcome TEXT NOT NULL, critique TEXT,
  related_memory_id INTEGER REFERENCES memories(id) ON DELETE SET NULL,
  confidence REAL DEFAULT 0.5 NOT NULL, last_accessed_at TEXT, access_count INTEGER DEFAULT 0 NOT NULL,
  created_at TEXT DEFAULT (datetime('now')) NOT NULL, UNIQUE(session_id, seq));
CREATE TABLE session_guide_usage (session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  guide_id INTEGER NOT NULL REFERENCES guides(id) ON DELETE CASCADE, PRIMARY KEY (session_id, guide_id));
CREATE TABLE session_memory_links (session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE, interaction_type TEXT NOT NULL,
  PRIMARY KEY (session_id, memory_id, interaction_type));
CREATE TABLE feedback_log (id INTEGER PRIMARY KEY AUTOINCREMENT,
  memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE, useful INTEGER NOT NULL,
  context TEXT, created_at TEXT DEFAULT (datetime('now')) NOT NULL);
CREATE TABLE improvement_suggestions (id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL, suggestion TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'offered', created_at TEXT DEFAULT (datetime('now')) NOT NULL,
  resolved_at TEXT);
CREATE TABLE fragments_archive (id INTEGER PRIMARY KEY AUTOINCREMENT, legacy_id TEXT,
  title TEXT NOT NULL, fragment TEXT NOT NULL, description TEXT, type TEXT NOT NULL, project TEXT,
  confidence REAL NOT NULL, source TEXT NOT NULL, context_tags TEXT, created_at TEXT, heat REAL,
  archived_at TEXT DEFAULT (datetime('now')) NOT NULL);
CREATE TABLE fragment_history (id INTEGER PRIMARY KEY AUTOINCREMENT,
  memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE, title TEXT NOT NULL,
  fragment TEXT NOT NULL, description TEXT, confidence REAL NOT NULL, type TEXT NOT NULL,
  changed_at TEXT DEFAULT (datetime('now')) NOT NULL);
CREATE TABLE memory_evidence (id INTEGER PRIMARY KEY AUTOINCREMENT,
  memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE, file_path TEXT NOT NULL,
  symbol TEXT, snippet TEXT NOT NULL, snippet_hash TEXT NOT NULL,
  created_at TEXT DEFAULT (datetime('now')) NOT NULL);
CREATE TABLE tfidf_cache (legacy_id TEXT PRIMARY KEY REFERENCES memories(legacy_id) ON DELETE CASCADE,
  content_hash TEXT NOT NULL, terms TEXT NOT NULL, updated_at TEXT DEFAULT (datetime('now')) NOT NULL);
CREATE VIRTUAL TABLE memory_fts USING fts5(title, fragment, description);
"#;

fn build_fixture_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
    // The dangling-relation row below simulates a backup written past the
    // live FK enforcement (upstream: external SQLite tools are undetectable),
    // so the fixture disables enforcement to insert it.
    conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
    conn.execute_batch(FIXTURE_DDL).unwrap();
    conn.execute_batch(
        "INSERT INTO schema_version(version) VALUES (1),(2),(3),(4),(5),(6),(7),(8);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memories (id, legacy_id, title, fragment, description, type, project, source, confidence, context_tags, access_count, last_accessed_at, negative_hits, positive_feedback, negative_feedback, quality_score, refinement_count, session_id, task_type, distill_candidate, related_guides, associated_with, created_at, updated_at) VALUES (1,'maaa00000001','Alpha','## Alpha','first fragment','fact','app','ai',0.9,'[\"rust\"]',5,'2026-09-01 10:00:00',3,2,1,0.7,1,'sabc00000001','research',1,'[\"rust-guide\"]','[\"mbbb00000002\"]','2026-09-01 09:00:00','2026-09-01 10:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memories (id, legacy_id, title, fragment, type, project, source, confidence, context_tags, parent_id, invalidated_at, last_accessed_at, created_at, updated_at) VALUES (2,'mbbb00000002','Beta','## Beta','lesson',NULL,'pager',0.5,'not-json',1,'2026-09-03 10:00:00','2026-09-02T10:00:00.000Z','2026-09-02 09:00:00','2026-09-02 10:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO relations (id, source_id, target_id, type, note, created_at) VALUES (1,1,2,'supports','because','2026-09-01 11:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO relations (id, source_id, target_id, type, note, created_at) VALUES (2,2,1,'supports','Auto-reverse of: because','2026-09-01 11:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO relations (id, source_id, target_id, type, created_at) VALUES (3,1,999,'contradicts','2026-09-01 12:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO guides (id, guide, category, description, mission, protocol, anti_patterns, pitfalls, usage_count, success_count, failure_count, last_used_at, depends_on, enables, source_memories, validated_by, created_at, updated_at) VALUES (1,'rust','lang','Rust manual','M','P','[\"a1\"]','[\"p1\"]',7,5,2,'2026-09-01 10:00:00','[\"cargo\"]','[\"wasm\"]','[\"maaa00000001\"]','[\"mzzz99999999\"]','2026-09-01 09:00:00','2026-09-01 10:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO guide_contexts (guide_id, context) VALUES (1,'ownership')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO guide_learnings (guide_id, learning) VALUES (1,'lifetimes')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO guide_memory_links (guide_id, memory_id, link_type) VALUES (1,1,'related'),(1,2,'source')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, task_type, technologies, initial_approach, final_approach, approach_changed, outcome, refinement_attempts, self_critique_count, lessons, tool_calls, memory_count, status, project, started_at) VALUES ('sabc00000001','research','[\"rust\"]','ia','fa',1,'success',2,1,'[\"l1\"]',9,2,'active','app','2026-09-01 09:30:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_attempts (session_id, seq, approach, outcome, related_memory_id, confidence, access_count, created_at) VALUES ('sabc00000001',1,'try a','promising',1,0.8,3,'2026-09-01 09:35:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_guide_usage (session_id, guide_id) VALUES ('sabc00000001',1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_memory_links (session_id, memory_id, interaction_type) VALUES ('sabc00000001',1,'read'),('sabc00000001',2,'created')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO feedback_log (memory_id, useful, context, created_at) VALUES (1,1,'ctx','2026-09-01 13:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO improvement_suggestions (session_id, suggestion, status, created_at, resolved_at) VALUES ('sabc00000001','try x','accepted','2026-09-01 14:00:00','2026-09-01 15:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO fragments_archive (legacy_id, title, fragment, type, project, confidence, source, context_tags, created_at, heat, archived_at) VALUES ('mold00000001','Old','Old frag','fact','app',0.1,'user','[\"old\"]','2026-01-01 10:00:00',0.5,'2026-09-01 10:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO fragment_history (memory_id, title, fragment, confidence, type, changed_at) VALUES (1,'Alpha-old','## Alpha-old',0.5,'fact','2026-09-01 08:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_evidence (memory_id, file_path, symbol, snippet, snippet_hash, created_at) VALUES (1,'src/lib.rs','main','fn main() {}','abc123','2026-09-01 10:00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tfidf_cache (legacy_id, content_hash, terms, updated_at) VALUES ('maaa00000001','h','{}','2026-09-01 10:00:00')",
        [],
    )
    .unwrap();
    conn.close().unwrap();
}

fn envelope_for(path: &Path, out: &Path) {
    let image = std::fs::read(path).unwrap();
    assert_eq!(&image[0..16], b"SQLite format 3\0");
    assert_eq!((image[18], image[19]), (1, 1), "DELETE mode image");
    std::fs::write(out, envelope_bytes_for(&image, 8)).unwrap();
}

/// Envelope JSON for an already-read image (the caller owns mode asserts:
/// (1,1) for supported snapshots, (2,2) for the WAL-rejection fixture).
fn envelope_bytes_for(image: &[u8], schema_version: u64) -> Vec<u8> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(image);
    let sha = hasher.finalize();
    let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, image);
    let envelope = serde_json::json!({
        "format": "lemma-backup",
        "format_version": 1,
        "lemma_version": "0.21.0",
        "schema_version": schema_version,
        "created_at": "2026-01-01T00:00:00.000Z",
        "database_sha256": hex,
        "database": encoded,
    });
    serde_json::to_vec(&envelope).unwrap()
}

/// Records, relations and guides from a Lemma-format image map into staging
/// with exact loss accounting.
#[test]
fn fixture_round_trips_with_exact_loss() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("lemma.db");
    build_fixture_db(&db_path);
    let before = std::fs::read(&db_path).unwrap();
    let backup_path = dir.path().join("test.lemma-backup");
    envelope_for(&db_path, &backup_path);

    let staging = import_to_json(&backup_path).unwrap();

    // Read-only source: the fixture image is byte-identical after import.
    assert_eq!(std::fs::read(&db_path).unwrap(), before);

    let v: serde_json::Value = serde_json::from_str(&staging).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["schema_version"], 8);
    assert_eq!(v["lemma_version"], "0.21.0");

    // The snapshot must deserialize as the interchange staging shape.
    let snapshot: ltmrs_domain::export::CanonicalExport =
        serde_json::from_value(v["snapshot"].clone()).unwrap();
    assert_eq!(snapshot.memories.len(), 2);
    assert_eq!(snapshot.relations.len(), 3);
    assert_eq!(snapshot.guides.len(), 1);
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.feedback.len(), 1);
    assert_eq!(snapshot.suggestions.len(), 1);
    assert_eq!(snapshot.projects.len(), 1);
    assert_eq!(snapshot.archives.len(), 1);
    assert_eq!(snapshot.history.len(), 1);

    // The manifest counts mirror the interchange manifest keys.
    let counts = &v["counts"];
    for (key, want) in [
        ("memories", 2),
        ("relations", 3),
        ("guides", 1),
        ("sessions", 1),
        ("feedback", 1),
        ("suggestions", 1),
        ("projects", 1),
        ("archives", 1),
        ("history", 1),
    ] {
        assert_eq!(counts[key], want, "count {key}");
    }

    // Digest binds the snapshot content.
    assert_eq!(v["digest"], snapshot.digest());

    // Memory 1: full field mapping.
    let alpha = snapshot
        .memories
        .iter()
        .find(|m| m.title == "Alpha")
        .unwrap();
    assert_eq!(
        alpha.external_alias.as_ref().unwrap().as_str(),
        "maaa00000001"
    );
    assert_eq!(alpha.description, "first fragment");
    assert!(matches!(
        alpha.fragment_type,
        ltmrs_domain::memory::FragmentType::Fact
    ));
    assert_eq!(alpha.project.as_deref(), Some("app"));
    assert!(matches!(
        alpha.source,
        ltmrs_domain::memory::MemorySource::Ai
    ));
    assert_eq!(alpha.confidence, 0.9);
    assert_eq!(alpha.tags, vec!["rust".to_string()]);
    assert_eq!(alpha.access_count, 5);
    assert_eq!(alpha.positive_feedback, 2);
    assert_eq!(alpha.negative_feedback, 1);
    assert_eq!(alpha.negative_hits, 3);
    assert_eq!(alpha.quality_score, Some(0.7));
    assert_eq!(alpha.refinement_count, 1);
    assert!(alpha.distill_candidate);
    assert_eq!(alpha.session_id.as_deref(), Some("sabc00000001"));
    assert_eq!(alpha.task_type.as_deref(), Some("research"));
    assert!(matches!(
        alpha.lifecycle,
        ltmrs_domain::memory::MemoryLifecycle::Live
    ));
    assert_eq!(alpha.evidence.len(), 1);
    assert_eq!(alpha.evidence[0].file, "src/lib.rs");
    // Related guides: the column value plus the guide_memory_links row.
    {
        let mut guides = alpha.related_guides.clone();
        guides.sort();
        assert_eq!(guides, vec!["rust".to_string(), "rust-guide".to_string()]);
    }
    // Associated memory resolves through the legacy id.
    let beta = snapshot
        .memories
        .iter()
        .find(|m| m.title == "Beta")
        .unwrap();
    assert_eq!(alpha.associated_with, vec![beta.id.to_string()]);
    // Parent link both ways.
    assert_eq!(
        beta.parent_id,
        None.map(|_: u128| alpha.id).or(Some(alpha.id))
    );
    assert_eq!(alpha.child_ids, vec![beta.id]);

    // Memory 2: unknown source defaults with loss, bad tags dropped with
    // loss, invalidation maps to lifecycle, ISO timestamp parses.
    assert!(matches!(
        beta.source,
        ltmrs_domain::memory::MemorySource::Ai
    ));
    assert!(beta.tags.is_empty());
    assert!(matches!(
        beta.lifecycle,
        ltmrs_domain::memory::MemoryLifecycle::Invalidated { .. }
    ));

    // Relations keep both directions (faithful 1:1); the dangling row rides
    // along for the restore quarantine.
    assert_eq!(
        snapshot
            .relations
            .iter()
            .filter(|r| r.source == alpha.id && r.target == beta.id)
            .count(),
        1
    );

    // Guide with contexts, learnings and source-memory links (column plus
    // the link table: the 'source' row must union in, not silently drop).
    let guide = &snapshot.guides[0];
    assert_eq!(guide.name, "rust");
    assert_eq!(guide.contexts, vec!["ownership".to_string()]);
    assert_eq!(guide.learnings, vec!["lifetimes".to_string()]);
    {
        let mut sources = guide.source_memories.clone();
        sources.sort();
        assert_eq!(sources, vec![alpha.id, beta.id]);
    }
    assert_eq!(guide.usage_count, 7);
    assert_eq!(guide.depends_on, vec!["cargo".to_string()]);
    assert_eq!(guide.enables, vec!["wasm".to_string()]);
    // Mission/protocol have no domain slot: folded into the description
    // with markers (disclosed, not silently dropped).
    assert!(guide.description.contains("Rust manual"));
    assert!(guide.description.contains('M'));

    // Session with attempts, guide usage and memory links (legacy ids kept).
    let session = &snapshot.sessions[0];
    assert_eq!(session.attempts.len(), 1);
    assert_eq!(session.guides_used, vec!["rust".to_string()]);
    assert_eq!(session.memories_read, vec!["maaa00000001".to_string()]);
    assert_eq!(session.memories_created, vec!["mbbb00000002".to_string()]);
    assert_eq!(session.technologies, vec!["rust".to_string()]);
    assert_eq!(session.lessons, vec!["l1".to_string()]);

    // Exact loss accounting: unknown source, bad tags, tfidf cache drop,
    // internal version-history rows.
    let loss = v["loss"].as_array().unwrap();
    let kinds: BTreeMap<String, u64> = loss
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap().to_string(),
                e["count"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(kinds.get("unknown_source"), Some(&1));
    assert_eq!(kinds.get("unparseable_tags"), Some(&1));
    assert_eq!(kinds.get("dropped_tfidf_cache"), Some(&1));
    assert_eq!(kinds.get("internal_schema_version"), Some(&8));
    assert_eq!(kinds.get("dangling_guide_link"), Some(&1));
    assert_eq!(kinds.get("folded_guide_manual"), Some(&1));
}

/// Shared import of the DELETE-mode fixture: parsed envelope, snapshot and
/// the source bytes the importer must leave untouched.
struct FixtureImport {
    _dir: tempfile::TempDir,
    backup_path: std::path::PathBuf,
    backup_before: Vec<u8>,
    v: serde_json::Value,
    snapshot: ltmrs_domain::export::CanonicalExport,
}

fn import_fixture() -> FixtureImport {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("lemma.db");
    build_fixture_db(&db_path);
    let backup_path = dir.path().join("test.lemma-backup");
    envelope_for(&db_path, &backup_path);
    let backup_before = std::fs::read(&backup_path).unwrap();
    let staging = import_to_json(&backup_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&staging).unwrap();
    assert_eq!(v["ok"], true);
    let snapshot: ltmrs_domain::export::CanonicalExport =
        serde_json::from_value(v["snapshot"].clone()).unwrap();
    FixtureImport {
        _dir: dir,
        backup_path,
        backup_before,
        v,
        snapshot,
    }
}

fn loss_kinds(v: &serde_json::Value) -> BTreeMap<String, u64> {
    v["loss"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap().to_string(),
                e["count"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// T-IMPORT-01 null quality: a NULL quality_score maps to None, never to an
/// implicit unknown-to-zero default; a present value maps exactly.
#[test]
fn null_quality_stays_null_not_zero() {
    let fix = import_fixture();
    let alpha = fix
        .snapshot
        .memories
        .iter()
        .find(|m| m.title == "Alpha")
        .unwrap();
    assert_eq!(alpha.quality_score, Some(0.7));
    // Beta's INSERT omits quality_score (NULL in the fixture image).
    let beta = fix
        .snapshot
        .memories
        .iter()
        .find(|m| m.title == "Beta")
        .unwrap();
    assert_eq!(beta.quality_score, None);
}

/// T-IMPORT-01 evidence blobs: the memory_evidence side table rides with its
/// memory, carrying symbol, snippet and hash (no domain slot for created_at,
/// which is loss-counted instead of kept).
#[test]
fn evidence_carries_symbol_snippet_and_hash() {
    let fix = import_fixture();
    let alpha = fix
        .snapshot
        .memories
        .iter()
        .find(|m| m.title == "Alpha")
        .unwrap();
    assert_eq!(alpha.evidence.len(), 1);
    assert_eq!(alpha.evidence[0].file, "src/lib.rs");
    assert_eq!(alpha.evidence[0].symbol.as_deref(), Some("main"));
    assert_eq!(alpha.evidence[0].snippet, "fn main() {}");
    assert_eq!(alpha.evidence[0].snippet_sha256, "abc123");
    let kinds = loss_kinds(&fix.v);
    assert!(
        kinds.contains_key("dropped_operational_fields"),
        "evidence created_at must be loss-counted, got: {kinds:?}"
    );
}

/// T-IMPORT-01 archives + sidecar history: the fragments_archive row and the
/// fragment_history row map with content, linked to the live memory.
#[test]
fn archive_and_history_rows_map_with_content() {
    let fix = import_fixture();
    assert_eq!(fix.snapshot.archives.len(), 1);
    let archived = &fix.snapshot.archives[0];
    assert_eq!(archived.title, "Old");
    assert_eq!(archived.fragment, "Old frag");
    assert!(matches!(
        archived.fragment_type,
        ltmrs_domain::memory::FragmentType::Fact
    ));
    assert_eq!(archived.heat, Some(0.5));
    assert_eq!(fix.snapshot.history.len(), 1);
    let history = &fix.snapshot.history[0];
    assert_eq!(history.title, "Alpha-old");
    assert_eq!(history.confidence, 0.5);
    let alpha = fix
        .snapshot
        .memories
        .iter()
        .find(|m| m.title == "Alpha")
        .unwrap();
    assert_eq!(history.memory_id, alpha.id);
}

/// T-IMPORT-01 suggestions: status maps through the CHECK-constrained
/// vocabulary ('accepted' here) and the resolution timestamp survives.
#[test]
fn suggestion_status_and_resolution_map() {
    let fix = import_fixture();
    assert_eq!(fix.snapshot.suggestions.len(), 1);
    let suggestion = &fix.snapshot.suggestions[0];
    assert!(matches!(
        suggestion.status,
        ltmrs_domain::session::SuggestionStatus::Accepted
    ));
    assert_eq!(suggestion.suggestion, "try x");
    assert!(suggestion.resolved_at.is_some(), "resolved_at must survive");
}

/// T-IMPORT-01 legacy reverse edges: the upstream AFTER INSERT trigger keeps
/// the forward row and its auto-reverse as a faithful 1:1 pair (never
/// re-synthesized, never dropped); the dangling row rides for quarantine.
#[test]
fn legacy_reverse_edges_ride_along_without_synthesis() {
    let fix = import_fixture();
    assert_eq!(fix.snapshot.relations.len(), 3, "no synthesis, no drop");
    let alpha = fix
        .snapshot
        .memories
        .iter()
        .find(|m| m.title == "Alpha")
        .unwrap();
    let beta = fix
        .snapshot
        .memories
        .iter()
        .find(|m| m.title == "Beta")
        .unwrap();
    let reverse = fix
        .snapshot
        .relations
        .iter()
        .find(|r| r.source == beta.id && r.target == alpha.id)
        .expect("auto-reverse row must ride along");
    assert!(matches!(
        reverse.relation_type,
        ltmrs_domain::relation::RelationType::Supports
    ));
    assert!(
        reverse
            .note
            .as_deref()
            .unwrap()
            .starts_with("Auto-reverse of: "),
        "upstream trigger note shape, got: {:?}",
        reverse.note
    );
}

/// T-IMPORT-02 write instrumentation: the envelope file itself — the actual
/// importer input — is byte-identical before and after import.
#[test]
fn envelope_file_byte_identical_after_import() {
    let fix = import_fixture();
    assert_eq!(std::fs::read(&fix.backup_path).unwrap(), fix.backup_before);
}

/// Minimal schema-v1 image: base tables only, memories without the v4/v5
/// additive columns, none of the v2+ side tables. All wanted guide/memory
/// columns exist since v1 upstream, so a v1 envelope must import (tolerant
/// missing-table path), not fail as corrupt.
const FIXTURE_DDL_V1: &str = r#"
CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at TEXT DEFAULT (datetime('now')) NOT NULL);
CREATE TABLE memories (
  id INTEGER PRIMARY KEY AUTOINCREMENT, legacy_id TEXT UNIQUE, title TEXT NOT NULL,
  fragment TEXT NOT NULL, description TEXT, type TEXT NOT NULL, project TEXT,
  source TEXT DEFAULT 'ai' NOT NULL, confidence REAL DEFAULT 0.5 NOT NULL,
  context_tags TEXT, access_count INTEGER DEFAULT 0 NOT NULL,
  last_accessed_at TEXT, negative_hits INTEGER DEFAULT 0 NOT NULL,
  positive_feedback INTEGER DEFAULT 0 NOT NULL, negative_feedback INTEGER DEFAULT 0 NOT NULL,
  quality_score REAL, refinement_count INTEGER DEFAULT 0 NOT NULL,
  parent_id INTEGER REFERENCES memories(id), session_id TEXT, task_type TEXT,
  distill_candidate INTEGER DEFAULT 0, related_guides TEXT, associated_with TEXT,
  last_refined TEXT, created_at TEXT DEFAULT (datetime('now')) NOT NULL,
  updated_at TEXT DEFAULT (datetime('now')) NOT NULL);
CREATE TABLE relations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
  target_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
  type TEXT NOT NULL, note TEXT, created_at TEXT DEFAULT (datetime('now')) NOT NULL,
  UNIQUE(source_id, target_id, type));
CREATE TABLE guides (
  id INTEGER PRIMARY KEY AUTOINCREMENT, guide TEXT NOT NULL UNIQUE, category TEXT NOT NULL,
  description TEXT NOT NULL, mission TEXT, protocol TEXT, anti_patterns TEXT, pitfalls TEXT,
  deprecated INTEGER DEFAULT 0 NOT NULL, superseded_by TEXT, usage_count INTEGER DEFAULT 0 NOT NULL,
  success_count INTEGER DEFAULT 0 NOT NULL, failure_count INTEGER DEFAULT 0 NOT NULL,
  last_used_at TEXT, depends_on TEXT, enables TEXT, source_memories TEXT, validated_by TEXT,
  last_refined TEXT, created_at TEXT DEFAULT (datetime('now')) NOT NULL,
  updated_at TEXT DEFAULT (datetime('now')) NOT NULL);
CREATE TABLE guide_contexts (id INTEGER PRIMARY KEY AUTOINCREMENT,
  guide_id INTEGER NOT NULL REFERENCES guides(id) ON DELETE CASCADE, context TEXT NOT NULL,
  UNIQUE(guide_id, context));
CREATE TABLE guide_learnings (id INTEGER PRIMARY KEY AUTOINCREMENT,
  guide_id INTEGER NOT NULL REFERENCES guides(id) ON DELETE CASCADE, learning TEXT NOT NULL,
  UNIQUE(guide_id, learning));
CREATE TABLE guide_memory_links (guide_id INTEGER NOT NULL REFERENCES guides(id) ON DELETE CASCADE,
  memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE, link_type TEXT NOT NULL,
  PRIMARY KEY (guide_id, memory_id, link_type));
CREATE TABLE sessions (
  id TEXT PRIMARY KEY, task_type TEXT, technologies TEXT, initial_approach TEXT,
  final_approach TEXT, approach_changed INTEGER DEFAULT 0, outcome TEXT,
  refinement_attempts INTEGER DEFAULT 0, self_critique_count INTEGER DEFAULT 0,
  lessons TEXT, tool_calls INTEGER DEFAULT 0 NOT NULL, memory_count INTEGER DEFAULT 0 NOT NULL,
  status TEXT DEFAULT 'active', project TEXT,
  started_at TEXT DEFAULT (datetime('now')) NOT NULL, ended_at TEXT);
CREATE TABLE session_guide_usage (session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  guide_id INTEGER NOT NULL REFERENCES guides(id) ON DELETE CASCADE, PRIMARY KEY (session_id, guide_id));
CREATE TABLE session_memory_links (session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE, interaction_type TEXT NOT NULL,
  PRIMARY KEY (session_id, memory_id, interaction_type));
CREATE TABLE feedback_log (id INTEGER PRIMARY KEY AUTOINCREMENT,
  memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE, useful INTEGER NOT NULL,
  context TEXT, created_at TEXT DEFAULT (datetime('now')) NOT NULL);
"#;

/// T-IMPORT-01 "every supported Lemma fixture": the oldest supported schema
/// (v1) imports through the tolerant missing-table path.
#[test]
fn schema_v1_fixture_imports_without_newer_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("lemma-v1.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
        conn.execute_batch(FIXTURE_DDL_V1).unwrap();
        conn.execute("INSERT INTO schema_version(version) VALUES (1);", [])
            .unwrap();
        conn.execute(
            "INSERT INTO memories (id, legacy_id, title, fragment, type, created_at, updated_at) VALUES (1,'mold00000001','Ancient','## Ancient','fact','2020-01-01 10:00:00','2020-01-01 10:00:00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO guides (id, guide, category, description, created_at, updated_at) VALUES (1,'old','lang','Old manual','2020-01-01 10:00:00','2020-01-01 10:00:00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, status, started_at) VALUES ('sold00000001','active','2020-01-01 10:00:00')",
            [],
        )
        .unwrap();
        conn.close().unwrap();
    }
    let backup_path = dir.path().join("v1.lemma-backup");
    std::fs::write(
        &backup_path,
        envelope_bytes_for(&std::fs::read(&db_path).unwrap(), 1),
    )
    .unwrap();
    let staging = import_to_json(&backup_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&staging).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["schema_version"], 1);
    let snapshot: ltmrs_domain::export::CanonicalExport =
        serde_json::from_value(v["snapshot"].clone()).unwrap();
    assert_eq!(snapshot.memories.len(), 1);
    assert_eq!(snapshot.guides.len(), 1);
    assert_eq!(snapshot.sessions.len(), 1);
    assert!(snapshot.archives.is_empty());
    assert!(snapshot.history.is_empty());
    let kinds = loss_kinds(&v);
    assert_eq!(kinds.get("internal_schema_version"), Some(&1));
    assert!(
        !kinds.contains_key("dropped_tfidf_cache"),
        "absent-at-v1 tables are tolerated, not loss-counted: {kinds:?}"
    );
}

/// T-IMPORT-02 WAL rejection: a raw WAL-mode (2,2) image — which the upstream
/// writer never emits (it normalizes its private serialized copy to (1,1)) —
/// fails safely with an actionable error instead of importing a snapshot
/// whose un-checkpointed frames live outside the image.
#[test]
fn wal_mode_image_rejected_with_actionable_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("live-wal.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (id INTEGER PRIMARY KEY, title TEXT NOT NULL);
             INSERT INTO memories (title) VALUES ('wal-row');",
        )
        .unwrap();
        conn.close().unwrap();
    }
    let image = std::fs::read(&db_path).unwrap();
    assert_eq!((image[18], image[19]), (2, 2), "fixture must be WAL-mode");
    let backup_path = dir.path().join("wal.lemma-backup");
    std::fs::write(&backup_path, envelope_bytes_for(&image, 8)).unwrap();
    let err = import_to_json(&backup_path).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("WAL"), "must name WAL mode, got: {text}");
    assert!(
        text.contains("checkpoint") || text.contains("serialize"),
        "must say how to produce a supported snapshot, got: {text}"
    );
}

/// T-IMPORT-02 live-WAL snapshot: a live WAL-mode database with the writer
/// still open and un-checkpointed frames in the -wal sidecar snapshots
/// coherently — mirroring the upstream writer (serialize under transaction,
/// normalize the private copy to (1,1); the live database stays in WAL mode)
/// — and the importer touches neither the live files nor the envelope.
#[test]
fn wal_live_snapshot_imports_coherently_without_touching_source() {
    use sha2::Digest as _;
    let dir = tempfile::tempdir().unwrap();
    let live_path = dir.path().join("live.db");
    let writer = {
        let conn = rusqlite::Connection::open(&live_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(FIXTURE_DDL).unwrap();
        conn.execute(
            "INSERT INTO schema_version(version) VALUES (1),(2),(3),(4),(5),(6),(7),(8);",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memories (id, legacy_id, title, fragment, type, created_at, updated_at) VALUES (1,'mwal00000001','WalBase','## WalBase','fact','2026-09-01 09:00:00','2026-09-01 10:00:00')",
            [],
        )
        .unwrap();
        // Committed but un-checkpointed: lives in the -wal sidecar only.
        conn.execute(
            "INSERT INTO memories (id, legacy_id, title, fragment, type, created_at, updated_at) VALUES (2,'mwal00000002','WalFresh','## WalFresh','fact','2026-09-01 09:00:00','2026-09-01 10:00:00')",
            [],
        )
        .unwrap();
        conn
    };
    let wal_path = live_path.with_extension("db-wal");
    assert!(
        wal_path.exists(),
        "sidecar must be present for a live-WAL test"
    );
    assert!(
        std::fs::metadata(&wal_path).unwrap().len() > 0,
        "un-checkpointed frames must sit in the sidecar"
    );
    // Upstream-equivalent snapshot: serialize the live view, normalize the
    // PRIVATE copy to (1,1) (upstream backup.js snapshot(); the live db is
    // untouched by construction — serialize copies).
    let reader = rusqlite::Connection::open(&live_path).unwrap();
    let data = reader.serialize(rusqlite::MAIN_DB).unwrap();
    let mut image: Vec<u8> = data.iter().copied().collect();
    drop(data);
    drop(reader);
    assert!(
        image.windows(8).any(|w| w == b"WalFresh"),
        "coherent cut must include un-checkpointed WAL frames"
    );
    image[18] = 1;
    image[19] = 1;
    let backup_path = dir.path().join("wal-live.lemma-backup");
    std::fs::write(&backup_path, envelope_bytes_for(&image, 8)).unwrap();

    // Write instrumentation on everything the importer must not mutate.
    let hash = |p: &std::path::Path| {
        let mut h = sha2::Sha256::new();
        h.update(std::fs::read(p).unwrap());
        hex_of_bytes(&h.finalize())
    };
    let live_before = hash(&live_path);
    let wal_before = hash(&wal_path);
    let envelope_before = hash(&backup_path);

    let staging = import_to_json(&backup_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&staging).unwrap();
    assert_eq!(v["ok"], true);
    let snapshot: ltmrs_domain::export::CanonicalExport =
        serde_json::from_value(v["snapshot"].clone()).unwrap();
    assert!(
        snapshot.memories.iter().any(|m| m.title == "WalFresh"),
        "un-checkpointed row must survive the import"
    );

    assert_eq!(hash(&live_path), live_before, "live db untouched");
    assert_eq!(hash(&wal_path), wal_before, "wal sidecar untouched");
    assert_eq!(
        hash(&backup_path),
        envelope_before,
        "envelope file untouched"
    );
    // The writer staying open (and usable) proves the importer took no lock
    // on the live database: it never opens it.
    writer
        .execute(
            "INSERT INTO memories (title, fragment, type) VALUES ('After','## After','fact')",
            [],
        )
        .unwrap();
}

fn hex_of_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
