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

### WP-04 — Singleton daemon, IPC and session routing (core)
- `src/daemon/envelope.rs`: typed IPC envelope (protocol version, store
  generation, frontend/channel IDs, operation ID, session, retry epoch,
  deadline, scope, typed body) with bounded length-prefixed framing
  (MAX_FRAME_BYTES = 8 MiB); `IpcError`, `DomainRequest`/`IpcResponse`/
  `IpcResult`; protocol-version validation.
- `src/daemon/registry.rs`: `FrontendRegistry` binding a legacy session per
  `(frontend_id, channel_id)` — never a daemon-global session (RV-05);
  `start_session`/`end_session`/`record_attempt`/`resolve_session`/
  `bind_native_session`/`expire_leases`/`set_lease`; one channel's
  `session_end` cannot end another's.
- `src/daemon/runtime.rs`: private 0700 runtime dir, 0600 socket, OS singleton
  lock (flock) held for the daemon's lifetime; the lock is the ownership source
  of truth, so a stale socket is only removed once the lock is free; symlinked
  runtime paths are refused (T-SEC-01/02).
- `src/daemon/dispatcher.rs`: daemon-side domain dispatcher routing typed IPC
  requests to the `CanonicalRepository` (mutations + reads) and the registry
  (session ops); every request scoped by frontend/channel identity; shareable
  across connections via `Arc`/`Mutex`.
- `src/daemon/scheduler.rs`: bounded embedding scheduler — a dedicated worker
  owns a bounded queue and a synchronous `&mut self` model adapter, keeping
  inference off Tokio I/O workers; backpressure is a retryable `Busy`, never
  unbounded allocation (T-CONC-04).
- `src/daemon/limits.rs`: per-client quotas and resource budgets (max clients,
  per-client queue, in-flight storage, response bytes) with visible backpressure.
- `src/daemon/health.rs`: health/doctor output (readiness, store generation,
  projection state, resource budgets) without dumping memory contents.
- `src/daemon/server.rs`: daemon lifecycle tying the lock, socket, dispatcher,
  scheduler and quotas together with a bounded accept loop.
- Added `serde` derives to `Scope`, `MemoryPatch`, `ForgetMode`; `uuid` gained
  the `v5` feature (deterministic sub-IDs); `libc` added for the OS lock.
- Tests (29 new): envelope framing/limits, registry 32-channel isolation + no
  global session + lease expiry, runtime one-owner + stale recovery + symlink
  refusal, dispatcher channel-scoped session_end + reads + protocol check,
  scheduler bounding + backpressure + closed, limits quotas, health report,
  daemon lifecycle.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --lib` (118 passed, 0 failed).

- **Streamed large responses** (`envelope.rs`): `split_payload`/
  `write_response_payload`/`read_response_payload` stream any response across
  bounded frames (8-byte total-length header + per-chunk length prefixes);
  byte-exact reassembly, never silent truncation. Server write path uses it.
- **Cancellation policy** (`cancellation.rs`): `decide` maps (committed,
  canceled) to Abort / Complete / KeepReceipt — canceled-before-commit aborts
  (no memory), canceled-after-commit preserves the durable receipt.
- **Idle-exit** (`idle.rs`): `IdleExitTracker` exits only when no connections
  and the idle timeout elapsed; reconnects/activity reset the window.
- **Session persistence + reconnect** (`registry.rs`): `persist`/`load`
  serialize sessions, channel bindings and leases to JSON; a restart restores
  durable history; reconnect restores only a verified (frontend, channel)
  binding, never guessing.
- **Shutdown/restore coordination** (`server.rs`): `Daemon::shutdown` persists
  sessions and aborts the scheduler worker; `Daemon::start` reloads sessions;
  a committed receipt survives a dropped connection (T-CONC-04/T-REC-01).
- **Frontend-side rmcp boundary** (`frontend/mcp.rs`): `LtmrsFrontend`
  implements `ServerHandler`, terminates stdio, and routes tool calls to typed
  IPC envelopes via `IpcClient` (no domain logic duplicated); `route_tool` is
  pure and testable.
- **IPC client** (`client.rs`): `IpcClient` connects to the daemon socket,
  writes length-prefixed request frames, reads streamed responses, supports
  reconnect.
- Tests (24 new): streaming split/roundtrip, cancellation matrix, idle-exit
  window, session persist/load/reconnect-verify, shutdown persistence +
  receipt-survives-drop, frontend tool routing + envelope identity, IPC
  client roundtrip.

### WP-04 — Connect-time handshake and retry namespace issuance
- `src/daemon/envelope.rs`: `WireMessage` (Handshake / Request(Box<IpcEnvelope>))
  and `WireReply` (Handshake / Response / Error(WireError));
  `HandshakeRequest`/`HandshakeResponse`, `validate_handshake`.
- `src/daemon/server.rs`: the first frame on every connection must be a
  handshake; wrong generation or request-before-handshake gets an error reply
  and is closed. Subsequent frames are requests only.
- `src/daemon/dispatcher.rs`: `handle_handshake` validates protocol version +
  store generation, then issues/refreshes the per-frontend retry namespace via
  `repo.issue_namespace()`.
- `src/daemon/client.rs`: `handshake()` on connect; stores `retry_epoch`;
  requests carry it. `src/frontend/mcp.rs` connects + handshakes lazily and
  stamps its envelopes with the handshake's epoch.
- `src/service/repository.rs`: `store_generation()` reads/writes a meta key
  (FIRST if absent), so every daemon on this store shares one generation.
- Tests: `handshake_rejects_wrong_generation`; all existing tests adapted to
  the tagged wire format.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (143 passed, 0 failed).

> WP-04 complete. All ten tasks done.

### WP-05 — Versioned Lance search projection (core)
- `src/domain/projection.rs`: durable desired-state job types (`ProjectionJob` with
  monotonic per-memory seq as the compare-and-clear token; tombstone flag).
- `src/service/repository.rs` + `repository_internal.rs`: add/update/forget write
  versioned jobs atomically in the command transaction (seq advances on every
  content mutation); `acknowledge_projection` is a true compare-and-clear keyed on
  seq, so a stale worker cannot clear newer work; `projection_job(s)`,
  `has_pending_projection`, `projection_lag`, `oldest_pending_age_millis`;
  `set_store_generation` for restore-driven generation switches.
- `src/search/row.rs`: search row schema (domain IDs, document revision, store
  generation, model fingerprint, chunk identity, project/type/date scope, rendered
  text, nullable embedding); lexical-ready without a vector; null-vector rows
  round-trip and filter correctly in the pinned Lance backend.
- `src/search/maintenance.rs`: scheduled maintenance worker (task 10) — a single
  background task runs budgeted Lance optimization + retention on an interval.
  Passes are sequential by construction (a slow pass delays the next tick rather
  than overlapping it); every action is bounded by an explicit `MaintenanceBudget`
  so nothing allocates unboundedly and no version another reader may hold is pruned.
- `src/search/table.rs`: `MaintenanceBudget` + `optimize_with_budgets` — compaction
  (thread/size caps), index optimize (fold unindexed tails) and retention pruning
  under explicit budgets; snapshot protection retains versions for at least
  `retain_millis`. `fts_query` degrades gracefully to empty when unbuilt.
- Daemon integration (`src/daemon/server.rs`): `DaemonConfig.search_path` +
  `maintenance` config; `start_maintenance()` idempotently spawns the worker on
  serve, and `shutdown()` aborts it so no orphan survives (design §7.2).
- `src/search/projector.rs`: worker that processes durable jobs end-to-end — reads
  canonical state fresh, validates revision + lifecycle under a per-entity
  publication lock, embeds only changed content, publishes idempotently; a stalled
  embedder still publishes the lexical row and leaves semantic work pending
  (T-PROJ-03); `publish_guarded` rejects stale revisions, non-recallable memories
  and rows from an inactive store generation (T-PROJ-02); tombstone jobs propagate
  deletion; `rebuild` never resurrects deleted generations; blue-green isolation
  by model fingerprint (new space builds without touching the old).
- Acceptance tests: T-PROJ-01 (kill between commit and ack keeps newer work
  pending; replay idempotent), T-PROJ-02 (stale publication refused after a
  generation switch), T-PROJ-03 (stalled embedder keeps lexical + direct reads),
  T-SEARCH-01 (empty DB, unbuilt FTS, null vectors, concurrent optimization).
- Maintenance tests: budgeted pass preserves data, config exposed for diagnostics,
  concurrent passes serialize without corruption, spawned worker runs repeated
  interval passes over live data; daemon test proves the worker spawns with a
  search path and aborts on shutdown.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (187 passed, 0 failed).

> WP-05 complete. All eleven tasks done.

### WP-06 — Candle embedding service and model qualification
- `src/embeddings/manifest.rs`: pinned E5-small artifact set (revision, per-file SHA-256
  digests, license) plus the `ModelRecipe` (query/passage prefixes, max_tokens=512,
  normalization, chunking policy). Only this qualified recipe is supported.
- `src/embeddings/artifacts.rs`: `ArtifactCache` with explicit fetch, digest
  verification on every load (tampering is a hard error), and an offline-only mode that
  never attempts downloads (`OfflineMissing` errors are actionable).
- `src/embeddings/bert_impl.rs`: Candle BertModel implementation matching the pinned
  config — multi-head attention with `.contiguous()` after transposes (batch>1 matmul
  requires it), attention-mask broadcasting [B,S]→[B,H,Sq,Sk], softmax over the key axis.
- `src/embeddings/e5_small.rs`: `E5SmallAdapter` — validation of architecture/tokenizer
  class against the recipe (rejects unsupported models with an actionable message),
  prefixing by role, batch padding + attention masks, masked mean pooling, L2
  normalization, finite-value checks, window enforcement (no silent truncation).
- `src/embeddings/e5_small.rs::chunk_passage`: tokenizer-length-aware greedy unit
  packing under the model window; chunks are verbatim spans of the fragment
  (`fragment[char_start..char_end] == text`, blank lines stay inside the slice) so parent
  identity is preserved exactly (T-EMB-03). Oversized single units get a disclosed hard
  split at the tokenizer boundary via binary search on char boundaries with a progress
  guard.
- `src/embeddings/worker.rs`: bounded synchronous worker thread owning the adapter
  (`&mut self`), dedicated OS thread off Tokio I/O workers; bounded queue is the
  backpressure point (full queue → retryable `Busy`, never unbounded allocation, RQ-22);
  generation-based `cancel_all`; stats accounting.
- `src/embeddings/service.rs`: async `EmbeddingService` facade over the worker handle
  (cloneable, cancellation via dropped futures).
- `src/embeddings/fixtures/reference_fixture.json` + `plans/models/e5-small-manifest.md`:
  reference vectors/tokens generated with the same prefix recipe as production, plus the
  model manifest documenting redistribution rights and the reference environment.
- Review fixes: batch>1 matmul crash (non-contiguous tensors after transpose) fixed in
  `bert_impl.rs`; oversized-unit "hard split" now actually splits at the tokenizer limit;
  near-limit boundary test added (508–512 tokens embed finite + normalized, over-limit
  rejected); `pop_timeout` waits only remaining time until its deadline instead of the
  full timeout each iteration.
- Tests: T-EMB-01 (tokenizer IDs/special tokens/prefixing vs reference), T-EMB-02
  (single/batched/empty/padded mixed-length/near-limit embeddings, normalization,
  padding correctness), T-EMB-03 (fact beyond first window retrieved in the correct
  chunk; verbatim offsets; hard-split oversized line fits the window).
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (207 passed, 0 failed).

> WP-06 complete. All ten tasks done.
