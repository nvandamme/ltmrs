# Slice C — Magic Context Bridge (Read-Only Import): Spec

## Goal

One-way import of memories / embeddings / git-commits from Magic Context's shared SQLite into ltmrs canonical store. Read-only against the external store: never write, checkpoint, VACUUM, or migrate it. Fail closed on version skew. Scope by `(harness, project_path → git-root → ltmrs scope)`.

## Non-goals

No writes to the MC store. No live sync / watch mode (one-shot preview→confirm). No embedding re-generation (imported vectors are opaque). No MCP tool in v1 (CLI-only; tool surface widens compat burden RQ-15).

## Architecture

Follow the `crates/lemma-import` precedent: the only `rusqlite` edge lives in an isolated crate excluded from the main workspace (`Cargo.toml` workspace `exclude`), loaded at runtime as cdylib (`crates/daemon/src/lemma_import.rs`). Do NOT put rusqlite in `crates/interchange`. New crate: `crates/mc-import` (excluded, same cdylib pattern), thin coordinator glue in `crates/interchange/src/restore/` (reuse `ImportCoordinator` lease pattern).

Spec lives in `plans/` (tracked).

## Global constraints

- Read-only analogue of RQ-26: source mtime/size unchanged after read (test asserts hash); WAL-mode live file refused.
- Fail-closed: unknown/newer MC version refuses naming supported vs found; no partial read.
- `cargo fmt -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` gate any implementation.
- No MC install, no network in tests (checked-in fixture DB + builder script).
- Plan-authority: Part I + WP + tests + ledger in one review.

## Interfaces (frozen; implementers use exactly these names)

```rust
// crates/mc-import/src/lib.rs (new)
pub const MC_FORMAT: &str;
pub const LATEST_SUPPORTED_VERSION: u64; // value TBD in Phase C1 (OQ-C1)
pub enum McImportError { /* version skew, WAL mode, scope escape, io */ }
pub fn open_readonly_copy(src: &Path) -> Result<TempCopy, McImportError>;
pub fn check_version(conn: &Connection) -> Result<(), McImportError>;

// crates/mc-import/src/scope.rs (new)
pub fn resolve_scope(harness: &str, project_path: &str) -> Result<ltmrs_service::...::Scope, McImportError>;

// crates/mc-import/src/map.rs (new)
pub struct McMemory { /* id, project_path, harness, content, embedding, created_at, metadata */ }
pub struct McCommit { /* sha, message, author, ts, files */ }
pub struct McImportReport { imported: ..., loss: Vec<McLoss>, digest: String }
```

- Reuse `crates/domain/src/project_id.rs` + service `Scope`/`validate_scope`. Canonicalize `project_path` → git-root (symlinks resolved); `../` traversal / symlink escape refused. Unknown harness refused (allowlist TBD OQ-C2).
- Row mapping: `McMemory` → `Memory` (external IDs preserved in `external_ids`, never overwrite ltmrs IDs; unknown fields → `loss`, RV-16: no coerced zero quality). `McCommit` → `Relation` edge or memory-attached provenance, never a fake memory (RV-12 direction/symmetry; exact edge kind OQ-C3).
- CLI: `mc-import preview|confirm` verbs; `preview(path) -> report` (no store touch) → `confirm(report_digest) -> receipt` (single-use TTL lease, atomic publish, generation bump + `invalidate_scan_cache`). Digest-bound (trust-but-verify).

## Behavior

Phase C1 reader + fence. Phase C2 scope mapping (mapping table in receipt). Phase C3 row mapping (digest-stable round-trip). Phase C4 CLI + atomic publish (double-confirm refuses). Phase C5 offline fixture tests (version matrix, WAL refusal, source-unchanged hash, scope-escape, loss accounting, digest stability, lease replay).

## Test plan

`crates/mc-import/tests/*.rs` + `tests/fixtures/build_fixture.py` (minimal MC-shaped SQLite, version-parameterized). Cases listed above; `cargo test -p <mc-import>` green offline.

## Plan-authority updates (one review)

Part I §-add (MC bridge isolation + RQ-26 read-only rule) + field-inventory table. Part II scope section + CLI/tool spec. Part III conformance cases (lease replay, digest mismatch). `traceability.json` new RQ entry; `conformance_matrix.json` scope-isolation cases.

## Open questions (Phase C1 research outputs, not implementation gaps)

OQ-C1: MC `LATEST_SUPPORTED_VERSION` value + schema location; older-version behavior (refuse vs loss-accounted). OQ-C2: harness vocabulary + `project_path` semantics; non-repo path handling. OQ-C3: table/column names + embedding dims (fixture schema is a guess until MC SQLite inspected). OQ-C4: CLI-only first (recommended).

## Spec self-review

- Placeholders: none — OQs are explicitly scoped Phase C1 research with decision defaults (fail-closed).
- Consistency: read-only holds end-to-end (copy-then-read, hash-checked, WAL-refused); loss accounting mirrors `StagingLoss` precedent.
- Scope: new crate + coordinator glue + CLI; no canonical pipeline changes.
- Ambiguity: edge kind for commits deferred to OQ-C3 with safe default (provenance attachment, not fake memory).
