# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`.

---

## Unreleased Commit

### WP-01 — Domain types and executable reference model
- Validated ID newtypes, external aliases, revisions, store generation, channel/session identities (`src/domain/id.rs`).
- Canonical record types preserving optional quality, lifecycle, evidence, archives, guide deps, attempts, suggestions (`src/domain/{memory,relation,guide,session,project,export}.rs`).
- Native DTOs separate from exact legacy wire DTOs; missing/null distinctions preserved (`src/domain/{wire,legacy}.rs`).
- `DomainCommand`, `CommandContext`, `CommandReceipt`, `DomainError`, `Scope`, `SnapshotToken` without Arrow/database types (`src/domain/command.rs`).
- In-memory sequential reference interpreter with deterministic IDs/clock as the concurrency-history oracle (`src/domain/interpreter.rs`).
- Graph endpoint, edge uniqueness, symmetry/direction, supersession-cycle and lifecycle predicates (`src/domain/graph.rs`).
- Canonical normalized export and digest ordering for round-trip/property tests (`src/domain/export.rs`).
- Legacy field map into canonical/envelope/derived/rejected categories (`src/domain/legacy.rs`).

### WP-02 — Backend capability and correctness gate
- Lance-only probe: create, conditional update, receipts, concurrency, snapshot, scalar query, kill/reopen durability, unenforced-PK metadata check (`src/storage/lance_backend.rs`).
- Fjall + Lance probe: optimistic cross-keyspace transactions, SSI write-write conflict detection, atomic merge, kill/reopen SyncAll durability, bounded retry under contention, one-winner absent-key create (`src/storage/fjall_backend.rs`).
- Arrow schemas for memories/relations/receipts (`src/storage/schema.rs`).
- **AD-01 DECIDED: Option B (Fjall + Lance)** — canonical state in Fjall keyspaces, Lance as search/projection store. Counterexample: Lance alone is not one-winner for concurrent absent-key creates. See `plans/AD-01_canonical_backend.md`.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --lib` (37 passed, 0 failed).
