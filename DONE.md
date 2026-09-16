# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`.

---

## Unreleased Commit

### WP-03 — Harden the canonical repository (core)
- `CanonicalRepository` over Fjall (`src/service/`): single `apply(ctx, cmd)`
  gateway centralizing command application, precondition validation and atomic
  receipt storage — the receipt commits in the same transaction as the command.
- Idempotency: durable `(store_generation, retry_epoch, operation_id)` key +
  request digest; same key+digest replays the recorded receipt; key reuse with
  different input is rejected.
- Retry namespaces: daemon-issued per-frontend namespaces with fixed 24h expiry;
  `issue_namespace`/`lookup_namespace`/`gc_expired` manage lifecycle; expired or
  unknown namespaces are refused as `StaleReplay`, not silently reworked.
- Conflict separation: storage conflicts retry from a fresh snapshot (bounded);
  stale `expected_revision` surfaces a domain conflict (not rebased); unknown
  commit outcomes resolve via the receipt and the same operation key.
- Uniqueness + referential/lifecycle invariants enforced in-transaction
  (memory, alias, edge); concurrency-safe supersession-cycle check.
- Deletion effects (design §5.3): hard delete severs adjacency; invalidation/
  archival preserve edges as history; evidence + guide links preserved on the
  tombstone; receipt history never removed; pending projections invalidated so
  a delayed worker cannot resurrect a deleted memory.
- Pending projections: `projections` keyspace; add-memory atomically records a
  pending projection; forget invalidates it.
- Feedback telemetry separation (RQ-17): observable counters/confidence are
  domain state on the memory record; the feedback event log is a separate
  `feedback_events` keyspace (diagnostic telemetry), keyed by operation so a
  replay cannot double-record.
- Migration runner (`src/service/migrations.rs`): versioned migration plan,
  safety rules (backup/validate/max-jump), refusal of newer/incompatible/
  corrupt schemas; stamps the schema version on open.
- Fault injection (RV-18): `FaultInjector` on the repository; injects unknown
  commit outcomes (crash before the durability barrier) and migration-step
  failures; store stays consistent, no partial writes, recoverable.
- Review fixes: receipt key now `(generation, frontend_id, retry_epoch, op)`
  so GC of one frontend's expired namespace cannot delete another frontend's
  receipts (cross-frontend leak); repository carries a `Clock` (SystemClock in
  prod, injectable) so namespace expiry checks use real time, not the deadline;
  `CanonicalExport::digest` normalizes before hashing (order-insensitive).
- Snapshot-consistent reads: multi-get, graph-neighbor, export traversal,
  projection status, feedback events.
- Tests (20 repository + 7 migration): atomic receipt, idempotent replay,
  key-reuse rejection, stale revision, supersession cycle, lifecycle
  transition, hard-delete adjacency, one-winner concurrent absent-key create,
  namespace epoch increment, unknown-namespace stale refusal, expired-namespace
  receipt GC, pending projection on add, hard-delete projection+edges,
  invalidate preserves edges, forget preserves receipts, feedback counters+event,
  feedback replay no double-record, injected unknown outcome consistency,
  injected migration fault atomicity, cross-frontend GC isolation.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --lib` (85 passed, 0 failed).

> WP-03 complete. All ten tasks done.
