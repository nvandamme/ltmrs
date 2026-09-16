# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`.

---

## Unreleased Commit

### WP-03 — Harden the canonical repository (core)
- `CanonicalRepository` over Fjall (`src/canonical/`): single `apply(ctx, cmd)`
  gateway centralizing command application, precondition validation and atomic
  receipt storage — the receipt commits in the same transaction as the command.
- Idempotency: durable `(store_generation, operation_id)` key + request digest;
  same key+digest replays the recorded receipt; key reuse with different input
  is rejected.
- Conflict separation: storage conflicts retry from a fresh snapshot (bounded);
  stale `expected_revision` surfaces a domain conflict (not rebased); unknown
  commit outcomes resolve via the receipt and the same operation key.
- Uniqueness + referential/lifecycle invariants enforced in-transaction
  (memory, alias, edge); concurrency-safe supersession-cycle check.
- Deletion effects: hard delete severs adjacency; invalidation/archival preserve edges.
- Snapshot-consistent reads: multi-get, graph-neighbor, export traversal.
- Schema version check on open; refuses unknown/newer incompatible schemas.
- Tests (8): atomic receipt, idempotent replay, key-reuse rejection, stale
  revision, supersession cycle, lifecycle transition, hard-delete adjacency,
  one-winner concurrent absent-key create.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --lib` (45 passed, 0 failed).

> Remaining WP-03 (not yet done): full migration runner, fixed-expiry retry
> namespaces + receipt GC, feedback/access telemetry separation, fault injection.
