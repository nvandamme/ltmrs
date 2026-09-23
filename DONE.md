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

### WP-07 — Retrieval, graph context and explanations (core)
- `src/retrieval/engine.rs`: the recall engine composing all stages — direct-ID
  and empty-query routing (separate from ranked search), lexical + dense legs
  executed separately, chunk collapse by parent, one-based RRF fusion, bundle
  resolution, bounded graph expansion, calibrated scoring, MMR, context budget,
  explanation. `QueryEmbedder` trait keeps inference off the retrieval path.
- `src/retrieval/scope.rs`: `EffectiveScope` resolved ONCE per call and applied
  to both legs (Lance filter), canonical hydration and every graph step (RV-13);
  project+global inheritance, type/date/confidence/lifecycle predicates;
  `to_lance_filter` escapes string literals (quote doubling).
- `src/retrieval/ranking.rs`: separate tested scorers — deterministic one-based
  `rrf_fuse`, `legacy_reference_score` (oracle only, RV-11), calibrated
  `native_score` (all components [0,1], frozen coefficients), `normalize_rrf`,
  `cosine` (missing/zero/non-finite vectors → 0.0, never NaN).
- `src/retrieval/graph_expansion.rs`: bounded expansion (depth/fan-out/node
  caps), edge-specific policies, per-node provenance paths, scope enforced on
  every hop, deduplicated hub contribution (no unlimited summation, RV-11).
- `src/retrieval/bundles.rs`: supersession chains resolved from the FULL
  relation graph (a stale candidate is caught even when its successor is not
  independently recalled); out-of-scope replacements never leak; conflict
  bundles preserved for MMR protection.
- `src/retrieval/mmr.rs`: greedy MMR on calibrated scores with protected-bundle
  priority, missing-vector lexical fallback, stable (score desc, ID asc)
  tie-breaking.
- `src/retrieval/context.rs`: budgets the actual serialized context,
  `AccountingMethod` labels approximate byte accounting when no tokenizer is
  known, conflict notice when a bundle cannot fully fit.
- `src/retrieval/explain.rs`: frozen `RETRIEVAL_PROFILE_VERSION = "1.0.0"`,
  per-candidate ranks/scores/graph paths/diversification decisions, readiness
  and partial flags.
- `src/search/table.rs`: `vector_query` (cosine, fingerprint-filtered, AD-04),
  `fts_query` accepts a scope filter, `fts_index_ready` reports the real index
  state for readiness.
- `src/service/repository.rs`: `all_relations` snapshot read for graph consumers.
- Review fixes: stale-only-recall supersession redirect (chain built from the
  full graph, not just recalled candidates); readiness `fts_ready` now checks
  the INVERTED index instead of row counts; Lance filter string-literal
  escaping; dense-leg model-fingerprint filter (AD-04); graph contribution 0.0
  for seeds; conflict notice checked against the final budgeted context.
- Tests: T-SCOPE-01/02, T-SEARCH-02 (identifiers, accents, mixed case,
  paths/underscores), T-RANK-01/02/03/04 (RRF arithmetic, scale separation,
  supersession/conflict bundles, no-answer), multilingual + identifier cases,
  stale-only-recall redirect, hub bound, scope-on-graph-step, escaping.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (275 passed, 0 failed).

> WP-07 complete. All twelve tasks done.

### WP-08 — Complete memory MCP contract (11 tools)
- `src/daemon/tools.rs`: all 11 WP-08 tool handlers executing against the
  canonical repository + search backend, shaping the frozen Lemma 0.21.0 wire
  contract (text + structuredContent + error flag).
- `memory_read`: single-ID detail, batch-ID, browse/query modes; pagination
  (limit/offset/has_more/next_offset); min_confidence/afterDate/beforeDate
  filters; `expand_graph` (depth ≤ 2); `explain` recall explanation
  (method/reason/provenance/freshness, pre-boost values); `response_format`
  json/markdown.
- `memory_add`: privacy redaction (DEV-002; `confirm=true` stores verbatim),
  dedup rejection (word-overlap ≥ 0.80), title/description auto-generation,
  distill-candidate flag for pattern/lesson, evidence attachment with SHA-256,
  topic-overlap auto-link (0.25–0.95, strongest match related_to).
- `memory_update`: confidence range validation, duplicate detection on
  fragment change, partial MemoryPatch.
- `memory_feedback`: upstream contract — positive = +0.015 confidence +
  access_count bump + positive_feedback++; negative = −0.02 confidence +
  negative_hits++ + negative_feedback++.
- `memory_forget`: hard delete, `invalidate=true` (hidden from recall,
  reversible), `consolidate=true` (down-weighted to 0.05, kept).
- `memory_merge`: ≥2 IDs validation, canonical Merge command, consolidate
  path records supersedes edges.
- `memory_relate`: type/source/target validation, duplicate-edge rejection,
  deterministic relation IDs.
- `memory_stats` / `memory_audit` / `memory_library`: stats (total,
  avg_confidence, by_source, by_project, low/high confidence), audit
  (duplicates, invalid confidence, missing text, dangling associations and
  relation edges), library snapshot (fragments paginated, guides, relations,
  signals, suggestions).
- `semantic_search`: search backend (hybrid/lexical) + lexical fallback,
  engine scores mapped to the legacy `score` display field, pagination.
- RQ-17 read side effects: `DomainCommand::Access` now carries the context
  tag; every `memory_read` persists confidence +0.015, access_count +1,
  last_accessed_at and the context tag via the canonical gateway BEFORE the
  success response (upstream boostOnAccess). `semantic_search` does not
  boost (matches upstream).
- `src/frontend/mcp.rs`: 11 frozen tools served verbatim (T-MCP-01) with
  annotations + output schemas; typed argument routing with validation
  (T-MCP-03); `instructions` field = frozen teaching template + dynamic
  memory index (project/global fragments, upstream buildInstructions);
  `tools/list_changed` notification after mutating tools (upstream
  notifyMemoryChange); snapshot prefetch for the dynamic index.
- `src/daemon/envelope.rs` + `dispatcher.rs`: `ListMemories` read-only IPC
  for the frontend snapshot.
- `src/compatibility/lemma/`: frozen schemas (tools_lemma_0_21_0.json),
  typed ToolArgs DTOs, privacy redaction (DEV-002).
- Tests: 40 new tool tests (T-MCP-01/02/03/04 evidence): 28 in
  `src/daemon/tools.rs` (add/read/update/feedback/forget/merge/relate/stats/
  audit/library/semantic_search, read side effects, explain, dedup, privacy,
  pagination, error classes) + 12 in `src/frontend/mcp.rs` (frozen-schema
  fidelity T-MCP-01, typed routing T-MCP-03, instructions index, notification
  triggers, result shaping).
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (321 passed, 0 failed).

> WP-08: eight of nine tasks done. Remaining: automated differential replay
> harness against upstream captures (task 9) — captures used as reference,
> contract encoded in behavioral tests, but no replay harness yet.

### WP-08 — Differential upstream wire harness (task 9)
- `tests/compat/lemma_0_21_0/rendering_fixture.json`: anonymized 177-fragment
  fixture derived from the real upstream Lemma 0.21.0 DB (structure preserved:
  legacy IDs, relations, confidence, dates, counts, projects, tags; private
  text replaced; 2 synthetic fragments cover the parent/child rendering path).
  Reference output generated by running the pinned upstream code.
- `src/daemon/tools.rs`: `differential_detail_matches_upstream_wire` and
  `differential_summary_matches_upstream_wire` replay the fixture through
  ltmrs's `render_detail`/`render_summary_index` and assert byte-for-byte
  equivalence with the upstream reference.
- Wire-contract bugs caught + fixed by the harness:
  1. Relation targets rendered as UUIDs instead of legacy IDs.
  2. `Refined from` / `Refined into` lines missing entirely.
  3. `Created:` rendered as epoch millis instead of a date-only string.
- `render_detail`/`render_summary_index` now take resolver closures (legacy
  ID lookup) instead of a repo handle, making them directly testable.
- DEV-004 recorded: ltmrs coerces non-canonical `source` strings to `ai`
  (upstream stores them verbatim); named deviation, not silent.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (324 passed, 0 failed).

### WP-08 — Traffic log differential oracle (complementary)
- `tests/compat/lemma_0_21_0/traffic_fixture.json`: differential fixture from
  actual upstream wire responses in traffic logs (8 fragments, pre-drift state).
  Captures what upstream ACTUALLY produced on the wire, not what it would
  produce now.
- `differential_detail_matches_traffic_log_wire`: verifies ltmrs reproduces
  the actual wire responses from logs, catching any drift between the log
  state and current DB state.
- Two complementary oracles now exist:
  1. `rendering_fixture.json` (177 fragments): regenerated from current DB
     via upstream code — verifies ltmrs matches upstream code behavior.
  2. `traffic_fixture.json` (8 fragments): actual wire responses from logs —
     verifies ltmrs matches what upstream actually produced.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (325 passed, 0 failed).

### WP-08 — Pure-function differential tests from upstream source
- `tests/compat/lemma_0_21_0/pure_functions.json`: oracle generated by
  running the pinned upstream code on targeted inputs (9 function
  categories: generateDescription, calculateQualityScore, calculateStats,
  formatStats, auditMemory, formatAuditReport, filterByProject,
  injectionScore, normalizeProjectKey).
- `tools/generate_differential_fixture.mjs`: regenerates the DB+logs
  differential fixture (node:sqlite, no external deps).
- `generate_description_matches_upstream` + `normalize_project_matches_upstream`
  differential tests replaying the pure-function oracle.
- **3 wire-contract bugs found + fixed in `generate_description`** by
  deriving edge cases from upstream:
  1. Used byte length (`.len()`) instead of UTF-16 code units — wrong for
     multi-byte text and could panic on non-char-boundary slices.
  2. Missing `.trim()` on the first sentence and the 80-unit truncation.
  3. Now replicates JavaScript UTF-16 semantics exactly (encode_utf16,
     from_utf16_lossy for lone surrogates).
 - Verified non-vacuous: reverting the fix makes the test fail.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (327 passed, 0 failed).

### WP-08 — Full pure-function differential coverage (9/9 categories)
- `tools/generate_pure_functions_oracle.mjs`: regenerates
  `tests/compat/lemma_0_21_0/pure_functions.json` by importing the pinned
  upstream code (via tsx) and running it on targeted inputs with a mocked
  clock (Date.now fixed at 2026-09-23T12:00:00Z) so time-dependent functions
  (calculateQualityScore, injectionScore) produce deterministic outputs.
  Numeric epoch millis are stored alongside ISO strings so the Rust tests
  need no date library.
- All 9 oracle categories now have a differential test in
  `src/daemon/tools.rs`:
  1. `generate_description_matches_upstream`
  2. `normalize_project_matches_upstream` (now keyed on `resolveProjectScope`,
     the function ltmrs's `normalize_project` actually implements)
  3. `calculate_stats_matches_upstream`
  4. `format_stats_matches_upstream`
  5. `audit_memory_matches_upstream`
  6. `format_audit_report_matches_upstream`
  7. `filter_by_project_matches_upstream`
  8. `calculate_quality_score_matches_upstream`
  9. `injection_score_matches_upstream`
- **3 wire-contract bugs found + fixed** by the new oracle:
  1. `filter_by_project` returned ALL fragments when no project was given;
     upstream returns only global fragments in that case. Fixed the match
     arm `(None, mp) => mp.is_none()`.
  2. `calculate_stats`/`format_stats` used `BTreeMap` (alphabetical key order)
     for by_source/by_project; upstream preserves insertion order. Added
     `indexmap` + `serde_json/preserve_order` and switched the aggregation
     maps to `IndexMap` so the serialized text matches upstream byte-for-byte.
  3. **CRITICAL (found in review, both review agents missed it):** the
     `memory_stats` tool uses SQL `getMemoryStats`, NOT the pure
     `calculateStats` my oracle was originally generated from. These diverge:
     - Project filter: SQL excludes globals; pure includes them
     - Global label: SQL uses `(global)`; pure uses `global`
     - Avg confidence: SQL is raw; pure rounds
     - Empty store: SQL returns `null` for low/high; pure returns `0`
     Fixed `calculate_stats` to match the real tool path (SQL semantics).
- **2 reference oracles added** in `src/compatibility/lemma/reference.rs`
  (mirroring `retrieval::ranking::legacy_reference_score`):
  - `calculate_quality_score`: faithful port of upstream's composite quality
    formula. ltmrs does NOT use this natively (design §10.3 uses its own
    calibrated scorer); retained as a test oracle only.
  - `injection_score`: faithful port of upstream's confidence×0.7 +
    recency×0.3 blend. Same rationale.
- Refactored `exec_memory_stats`/`exec_memory_audit` to extract the pure
  logic into `calculate_stats`/`format_stats`/`filter_by_project`/
  `audit_memory`/`format_audit_report`, making them directly testable against
  the oracle without a live repository.
- Verified non-vacuous: reverting the `filter_by_project` fix makes
  `filter_by_project_matches_upstream` fail.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (334 passed, 0 failed).
