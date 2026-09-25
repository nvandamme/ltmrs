# CHANGELOG.md

Work completed on previous commits, grouped by commit.
Content before `---` is instructions — do not modify. Add entries after the `---`.

---

## 0427c3a (2026-09-26) — IPC hardening: peer-cred, deadline, quotas, idle-exit, wire fix

- `src/daemon/server.rs`: same-UID peer-credential check before any frame
  (`peer_authorized` + `check_peer_cred`, strict incl. root); client
  register/unregister lifecycle (hoisted `ClientGuard` bound to the
  handshake ID, per-frame `frontend_mismatch` rejection); in-flight
  storage slots with balanced finish/dequeue; response byte budget with
  explicit refusal (control replies bypass); idle-exit serve mode
  (`idle_timeout_millis`, 0 = forever) with panic-safe disconnect notes.
- `src/daemon/dispatcher.rs`: past-deadline refusal at handle entry.
- `src/daemon/limits.rs`: `unregister_client` (absent is no-op).
- `src/daemon/envelope.rs`: `WireReply` adjacently tagged — the inner
  `WireError.kind` collided with internal tagging, making every rejection
  unparseable (pre-existing bug, first tested here).
- Review fixes: guard-scope lifetime, frame-ID binding, refcount direction,
  Drop-time panic safety; lifecycle + mismatch tests added.
- Tests: peer pure+live, deadline past/future, unregister, client-limit,
  slot lifecycle, mismatch, storage-busy, response-cap, idle-exit (11 new).
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (412 passed, 0 failed).

## 6958b5c (2026-09-26) — golden wire pins and error envelopes for memory tools

- `src/daemon/tools.rs`: exact-text regression pins for memory_stats +
  memory_audit on fixed fixtures (deterministic, double-run; honestly
  labeled pins, not upstream oracles — `legacy_oracle` stays `not_run`
  except memory_read); error envelopes for unknown IDs
  (feedback/forget/relate) and out-of-range confidence, each verified to
  error for the right reason.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (401 passed, 0 failed).

## b5891bf (2026-09-26) — per-request E5 fingerprint plumbing

- `src/embeddings/e5_small.rs::E5_SMALL_FINGERPRINT` (value 1, matching the
  de-facto test convention; no space change); `ModelFingerprint::new` is now
  `const fn` (behavior-neutral).
- `src/daemon/tools.rs`: `recall_browse` + `exec_semantic_search` pin the
  E5 fingerprint when a backend is attached (dense leg runs; empty tables
  fall back gracefully, unchanged).
- Tests: attached backend runs the dense leg once (counter-pinned,
  RED-verified) with fallback results intact.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (398 passed, 0 failed).

## 03871eb (2026-09-26) — bounded-worker query routing over async bridge

- `src/search/backend.rs`: `ServiceQueryEmbedder` implements the engine
  async seam directly (no `block_on` anywhere in the query path — an early
  sync-bridge draft nested it and would panic on any dense query);
  `Busy` maps to retryable `busy:`-prefixed errors; `SearchBackend` holds
  the async trait (sync adapter kept public for sync providers/tests).
- `src/daemon/server.rs`: E5 arm builds the worker service; daemon owns +
  shuts it down explicitly.
- Tests: role evidence, shutdown error, Busy retryable (deterministic),
  no-nesting dense retrieve_sync with embed-call counter.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (397 passed, 0 failed).

## eb4cfcb (2026-09-26) — table-measured generation convergence check

- `src/search/projector.rs::verify_generation_converged`: true when every
  live recallable memory has ≥1 row in the generation (subset, not
  equality — tombstones excluded; lexical rows count, vectors are a
  readiness dimension; empty-live vacuously true; generation-scoped,
  fingerprint-blind by the same-space rule). Operator calls it after the
  final build instead of trusting the reported numerator alone.
- Tests: detects-missing (false→true across convergence), scoped +
  ignores-deleted.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (393 passed, 0 failed).

## 2a1296f (2026-09-26) — per-generation build-dirtiness for cutover safety

- `src/domain/projection.rs`: `GenerationRecord.build_dirty` (fail-closed
  serde default: pre-upgrade records decode dirty, must be re-reported).
- `src/service/repository_internal.rs`: `CommandState` carries the
  `generations` keyspace; `mark_build_dirty()` set atomically with memory
  add / content update / forget / merge (merge also gained the missing
  result pending job + source tombstone jobs per §5.3, and project-only
  updates now count as content since project is Lance-indexed);
  `src/domain/interpreter.rs` mirrors the project gate (oracle parity).
- `src/service/repository.rs`: `note_generation_progress` clears dirty
  (trusted attestation, documented); `activate_generation` refuses dirty
  Ready pipelines ("rebuild and re-note first"), Retired rollback exempt;
  `abandon`/restore preserve the flag (moot under the exemption).
- Reviews (formal + functional): merge job/dirty hole, project-column gap,
  dirty-Retired stuck state, fail-closed default, trust-boundary docs —
  all fixed; mutant-killed (dirty-message assert, Retired exemption).
- Tests: mid-build add/forget block activation until re-note, merge
  jobs+dirty, project-only enqueues+dirties, confidence-only stays clean,
  old-record decodes dirty, dirty-Retired still rolls back.
- Explicit follow-ups (not claimed): per-memory pending verification,
  table-measured numerator (closed by eb4cfcb above), merge required
  edges/references (§5.3).
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (391 passed, 0 failed).

## d305be3 (2026-09-25) — wire generation reaper into maintenance passes

- `src/search/maintenance.rs`: `MaintenanceScheduler::with_repo` + explicit
  `generation_retain_millis` config (7-day default); one shared
  `maintenance_pass` body for `run_pass` and the spawned loop; unwired
  passes behave exactly as before (paired wired/unwired tests).
- `src/daemon/dispatcher.rs` + `server.rs`: new `repo_arc()` accessor;
  `start_maintenance` attaches the dispatcher repository so daemon passes reap.
- Tests: wired pass reaps expired retired rows, unwired pass skips (2 new).
- Review passes (formal + functional): PASS with zero findings.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (383 passed, 0 failed).

## c970d3a (2026-09-25) — review gaps: adapter gate, ledgers, session_stats deadlock

- `src/storage/mod.rs`: losing Lance canonical probe gated behind
  `#[cfg(test)]` per AD-01/WP-02 D4 (production tree clean; 9 counterexample
  tests still run under `cargo test`).
- `plans/conformance_matrix.json`: 15 WP-09 tools to `executed_passed` on
  schema/defaults/output/state (behavioral tests verified);
  `semantic_search` owner WP-09 → WP-08.
- `plans/traceability.json`: RQ-06 evidence now describes the shipped 24h
  retry namespaces + `gc_expired`.
- `src/daemon/tools.rs`: fixed `exec_session_stats` registry double-lock
  deadlock — chaining `disp.registry()` guards in one expression hung every
  call with an active session (non-reentrant Mutex); single guard instead,
  all other call sites audited safe. Backing test for empty/active/
  completed states (the conformance flip that caught it).
- Review passes (formal + functional): one HIGH finding (unbacked
  `session_stats` flip) fixed with the test above.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (381 passed, 0 failed).

## d6c9df3 (2026-09-25) — chunker-version column with version-scoped purge

- `src/search/row.rs` + `table.rs`: `chunker_version` String appended last
  (slot 13, positional reads stable); `search_schema`, `row_batch`,
  `batches_to_rows`; shared open/refresh gate (name + slot + type +
  nullability) — pre-versioning tables fail fast with a rebuild directive
  (projection is derived state).
- `src/search/projector.rs`: `Embedder::chunker_version` (default
  `single-chunk-v1`); every row stamped incl. the empty-chunk fallback.
  Corrected unit-1 rule: same model+recipe is one vector space, so policy
  changes bump the version, never the fingerprint.
- `src/search/backend.rs`: `E5_CHUNK_VERSION = "e5-chunks-v1"` for the recipe
  mapping; distinctness pinned by test.
- `src/search/table.rs`: same-revision republish under a new version purges
  the old policy's chunk ids (RQ-08, mutant-verified).
- Review fixes: orphan purge, refresh gate sharing, strict slot/type gate,
  test-double version attribution, schema slot pin, ranking-across-versions
  doc softening (WP-12 calibration owns it).
- Tests: field, round-trip, open rejection, plumbing, const distinctness,
  slot pin, policy purge (7 new).
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (380 passed, 0 failed).

## 9bc6be1 (2026-09-25) — blue-green generation cutover with atomic activation

- `src/domain/projection.rs`: `GenerationStatus`
  (Staged/Building/Ready/Active/Retired) + `GenerationRecord` (fingerprint
  Option for pre-record eras).
- `src/service/repository.rs`: `generations` keyspace; stage (watermark
  denominator snapshotted, one pipeline at a time), progress notes
  (Staged→Building→Ready), single-transaction activation (live watermark
  re-check, pointer flip, predecessor retired, pre-record predecessor
  auto-retired); rollback re-activates retained rows (watermark-exempt);
  abandon path; restore retires all live records; Active-preferring
  `store_generation`; idempotent re-activation; `checked_add` on the counter.
- `src/retrieval/engine.rs`: `store_generation` is now Option (None = active
  pointer resolved per request, Some = pinned); repository errors propagate.
- `src/search/projector.rs`: staged-generation writes allowed only under
  construction AND fingerprint match (`generation_under_construction` moved
  to the repository).
- `src/search/maintenance.rs`: `reap_retired_generations` (expired Retired
  only, active never touched).
- Review fixes: rebuild-based R6 (pre-activation invisibility proven with
  ranked-path query; list mode is generation-agnostic by design), abandon
  path, restore sweep, fingerprint binding (mutant-killed), idempotency,
  overflow guard, engine error propagation. Mid-build-write window and
  table-measured numerator recorded as explicit follow-ups (per-generation
  pending work).
- Tests: atomic cutover, stale watermark, rollback, interrupted build
  (reopen), abandon, restore sweep, idempotent activate, wrong-fingerprint
  refusal, active-pointer resolution, reaper expiry (10 new).
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (373 passed, 0 failed).
- Note: WP-03 concurrent-cycle barrier test rode along in `repository.rs`
  (same-file adjacency to the cutover methods).

## 281f7ff (2026-09-25) — E5 embedding bridges plus daemon wiring for dense retrieval

- `src/search/backend.rs`: `e5_chunks_to_text_chunks` mapping (verbatim
  spans re-prefixed, offsets shifted to rendered coordinates); `impl
  Embedder for E5SmallAdapter` (Passage role, window errors surface as
  pending — never silent truncation); `impl QueryEmbedderProvider for
  Mutex<E5SmallAdapter>` (Query role; type separation enforces prefix
  asymmetry); bounded-worker routing noted as deferred (RQ-22).
- `src/daemon/server.rs`: `EmbeddingMode` (Disabled default / E5SmallCached
  with fail-fast artifact load, search_path validated first); `Daemon::start`
  is now async (removes the `block_on` panic class); `SearchBackend`
  attached via the pre-built `with_search` seam; new `DaemonError::Embedding`.
- `src/daemon/tools.rs`: `recall_browse` falls back to the snapshot scan on
  empty backend results (fresh-E5-start regression test, non-vacuous).
- `src/daemon/client.rs`: async-start call-site churn (one `.await` each).
- Review fixes: empty-table recall regression, async start, query-bridge
  role-equivalence + asymmetry test, narrowed wiring-only claims, reworded
  model-state doc, validation order. Inherent `embed` shadows the trait seam
  (tests use qualified syntax); `retrieve_sync` requires sync context
  (tests use `spawn_blocking` like the dispatcher).
- Tests: chunk mapping, Passage/Query bridge equivalence + asymmetry,
  disabled default, fail-fast, empty-backend fallback (7 new).
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (363 passed, 0 failed).
- Deferred: projection loop writing dense vectors, per-request fingerprint
  plumbing, bounded-worker query routing.

## fa9cd7c (2026-09-25) — chunk-aware search projection with guarded multi-row publish

- `src/search/projector.rs`: `Embedder::chunk_text` (default single unit,
  byte-identical rows) + `TextChunk` (rendered-coordinate spans);
  `render_chunk_rows` shared by `process_job` + `rebuild`;
  `publish_rows_guarded` validates once, publishes the chunk set together,
  rejects mixed sets with `Validation` (not `debug_assert`), empty input
  returns `Ok(false)`; all-or-pending ack preserves the
  Published/SemanticPending contract.
- Tests: one-row-per-chunk (+span coverage asserts), rebuild-supersedes-as-
  unit, stalled-leaves-lexical, mixed-partial-stays-pending (all-vs-any swap
  verified to fail) (4 new).
- Review fixes: E5-mapping doc corrected (fragment-relative + fingerprint
  precondition, later superseded by the version column); test-helper
  char-boundary fix; chunker-version/atomicity limits recorded as explicit
  follow-ups, not claimed.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (356 passed, 0 failed).

## 3a31362 (2026-09-24) — complete WP-09 guides, sessions and intelligence

- `src/compatibility/lemma/tools_lemma_0_21_0.json`: 15 new frozen tool schemas
  (7 guide, 5 session/suggestion, 3 intelligence) added to the 26-tool surface.
- `src/compatibility/lemma/tool_args.rs` + `src/frontend/mcp.rs`: 15 typed
  argument DTOs + routing arms + parse functions (frozen-schema fidelity).
- `src/compatibility/lemma/intelligence.rs`: intelligence layer porting
  upstream conflict.ts / proactive.ts / scoring.ts / session-analytics.ts —
  conflict detection (negation + contradiction signals, topic overlap),
  proactive suggestions (stale/orphan/deprecated/unpracticed/hot-distill/
  low-quality), project analytics (growth rate, skill coverage, insights,
  health score). Read-only; never mutates canonical state.
- `src/daemon/tools.rs`: all 15 WP-09 handlers:
  - Guide: get (task suggestions / single / category list), practice
    (usage + success/failure + session validation links + hook suggestions),
    create (existing/similar update path), distill (fragment → guide learning,
    related_guides + distill_candidate clear), update (rename/category/
    description/anti-patterns/pitfalls/depends_on/enables/superseded/deprecated),
    forget, merge (≥2 sources, usage sum, ref merge).
  - Session: start (per-channel abandon+create, decay attempts, guide
    suggestions, pre-load boost, continuity recall, pending suggestions),
    attempt (redact, seq, self-critique/refinement counters), end (guide
    outcome eval on failure, improvement suggestions, session review),
    stats, suggestion_respond (accept/dismiss + attempt boost/penalize).
  - Intelligence: conflict_scan, proactive_analysis, project_analytics.
- `src/domain/command.rs` + `repository_internal.rs` + `interpreter.rs`:
  `DomainCommand::BoostConfidence` (+0.02 pre-load boost, upstream parity —
  distinct from `Access` +0.015).
- `src/daemon/registry.rs`: session lifecycle — `start_legacy_session`,
  `track_guide_used`, `track_memories_read`, `track_memories_created`,
  `decay_attempts`, `boost_attempt`, `penalize_attempt`, `all_sessions_owned`,
  `session_mut`. Per-channel isolation preserved (RV-05).
- `src/service/repository.rs`: guide/suggestion keyspaces + accessors
  (`get_guide`/`put_guide`/`delete_guide`/`get_guides`, suggestion CRUD,
  `put_memory_direct`).
- `src/domain/session.rs`: Session/Attempt/Suggestion extended with
  technologies, initial_approach, guides_used, memories_read/created,
  refinement_attempts, self_critique_count, attempt seq/confidence.
- memory_add links the created fragment to the channel's active session
  (session_id / task_type / memories_created).
- Guide rename/forget/merge maintain memory `related_guides` references
  (`rename_guide_in_memories`, `remove_guide_from_memories`).
- Review fixes (2 passes): skill-coverage category + quality-score staleness +
  sorts + insights date (intelligence.rs); pre-load boost 0.015→0.02;
  session_end success-rate check scoped to failure; memory_add session link;
  session_attempt char-boundary-safe preview; guide memory-reference
  maintenance.
- Tests: 17 new (guide CRUD/practice/distill/forget/merge, session lifecycle,
  pre-load boost 0.02, memory-reference rename/forget, conflict detection,
  proactive analysis, project analytics, suggestion_respond, session_end
  retry-safety no-double-count).
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (351 passed, 0 failed), `cargo build --release` OK.
- WP-09: seven of ten tasks done. Remaining: (3) virtual-session lifecycle —
  ltmrs uses per-channel traced sessions in the registry instead (documented
  deviation, RQ-05); (8) dense-search candidate proposal — guide suggestions
  use token matching (advisory, not proof), dense leg deferred; (10) extended
  upstream conformance traces across the full recall→act→persist workflow —
  differential replay for the 15 WP-09 tools deferred (behavioral tests encode
  the contract instead).

## 339fa58 (2026-09-23) — complete WP-08 memory MCP contract and differential wire harness

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
- Differential wire harness (task 9): `tests/compat/lemma_0_21_0/
  rendering_fixture.json` (anonymized 177-fragment fixture from the real
  upstream DB) replayed through ltmrs's `render_detail`/
  `render_summary_index` and asserted byte-for-byte against the upstream
  reference. Caught + fixed: relation targets rendered as UUIDs instead of
  legacy IDs; `Refined from`/`Refined into` lines missing; `Created:`
  rendered as epoch millis instead of a date-only string.
- Traffic-log differential oracle: `tests/compat/lemma_0_21_0/
  traffic_fixture.json` (actual upstream wire responses from logs) — a
  complementary oracle verifying ltmrs matches what upstream actually
  produced.
- Pure-function differential tests: `tests/compat/lemma_0_21_0/
  pure_functions.json` oracle (9 categories) generated by running the pinned
  upstream code with a mocked clock; 9 differential tests in `tools.rs`.
  Caught + fixed 3 `generate_description` bugs (UTF-16 code units vs bytes;
  missing trim + 80-unit truncation) and 3 wire-contract bugs (`filter_by_
  project` global scope; `calculate_stats`/`format_stats` insertion order via
  `indexmap`; `memory_stats` uses SQL semantics, not the pure function).
- `src/compatibility/lemma/reference.rs`: `calculate_quality_score` and
  `injection_score` retained as test oracles only (not used natively).
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (334 passed, 0 failed).
- WP-08: eight of nine tasks done. Remaining: automated differential replay
  harness against upstream captures (task 9) — captures used as reference,
  contract encoded in behavioral tests, but no replay harness yet.

## 89f7e58 (2026-09-20) — complete WP-07 retrieval, graph context and explanations

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

## dcc2e99 (2026-09-18) — complete WP-06 Candle embedding service and model qualification

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

## 7cf7aff (2026-09-17) — complete WP-05 versioned Lance search projection

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

## dcdb257 (2026-09-17) — complete WP-04 singleton daemon, IPC and session routing

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
- Streamed large responses (`envelope.rs`): `split_payload`/
  `write_response_payload`/`read_response_payload` stream any response across
  bounded frames (8-byte total-length header + per-chunk length prefixes);
  byte-exact reassembly, never silent truncation. Server write path uses it.
- Cancellation policy (`cancellation.rs`): `decide` maps (committed,
  canceled) to Abort / Complete / KeepReceipt — canceled-before-commit aborts
  (no memory), canceled-after-commit preserves the durable receipt.
- Idle-exit (`idle.rs`): `IdleExitTracker` exits only when no connections
  and the idle timeout elapsed; reconnects/activity reset the window.
- Session persistence + reconnect (`registry.rs`): `persist`/`load`
  serialize sessions, channel bindings and leases to JSON; a restart restores
  durable history; reconnect restores only a verified (frontend, channel)
  binding, never guessing.
- Shutdown/restore coordination (`server.rs`): `Daemon::shutdown` persists
  sessions and aborts the scheduler worker; `Daemon::start` reloads sessions;
  a committed receipt survives a dropped connection (T-CONC-04/T-REC-01).
- Frontend-side rmcp boundary (`frontend/mcp.rs`): `LtmrsFrontend`
  implements `ServerHandler`, terminates stdio, and routes tool calls to typed
  IPC envelopes via `IpcClient` (no domain logic duplicated); `route_tool` is
  pure and testable.
- IPC client (`client.rs`): `IpcClient` connects to the daemon socket,
  writes length-prefixed request frames, reads streamed responses, supports
  reconnect.
- Connect-time handshake: `WireMessage` (Handshake / Request(Box<IpcEnvelope>))
  and `WireReply` (Handshake / Response / Error(WireError)); the first frame
  on every connection must be a handshake; wrong generation or request-before-
  handshake gets an error reply and is closed. `handle_handshake` validates
  protocol + generation then issues/refreshes the per-frontend retry namespace
  via `repo.issue_namespace()`; `store_generation()` reads/writes a meta key
  (FIRST if absent).
- Tests (53 new): envelope framing/limits, registry 32-channel isolation + no
  global session + lease expiry, runtime one-owner + stale recovery + symlink
  refusal, dispatcher channel-scoped session_end + reads + protocol check,
  scheduler bounding + backpressure + closed, limits quotas, health report,
  daemon lifecycle, streaming split/roundtrip, cancellation matrix, idle-exit
  window, session persist/load/reconnect-verify, shutdown persistence +
  receipt-survives-drop, frontend tool routing + envelope identity, IPC
  client roundtrip, handshake_rejects_wrong_generation.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (143 passed, 0 failed).

## d5e8d40 (2026-09-16) — complete WP-03 canonical repository hardening

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

## 7cbc1b6 (2026-09-16) — domain model, reference interpreter, backend gate probes (WP-01, WP-02)

- WP-01: validated ID/alias/revision/generation identities; canonical record
  types; native + legacy wire DTOs; `DomainCommand`/`CommandContext`/
  `CommandReceipt`/`DomainError`/`Scope`/`SnapshotToken`; in-memory sequential
  reference interpreter (concurrency oracle); graph predicates; canonical export
  + digest; legacy field map (`src/domain/`).
- WP-02: Lance-only probe and Fjall+Lance probe (optimistic transactions, SSI
  conflict detection, atomic merge, SyncAll durability, bounded retry,
  one-winner create); Arrow schemas (`src/storage/`).
- **AD-01 DECIDED: Option B (Fjall + Lance)** — canonical state in Fjall
  keyspaces, Lance as search/projection store. See
  `plans/AD-01_canonical_backend.md`.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --lib` (37 passed, 0 failed).

## dd0be90 (2026-09-16) — close WP-00 gaps from plan review

- Added static tool-definition snapshot (`static/tools_static.json`), kept
  separate from the live `tools/list` (dynamic description injection).
- Recorded DB schema version (8) and static/live snapshot refs in
  `upstream-lock.json`.
- Added behavior classification (schema-documented / handler-defined /
  side-effect / apparent-defect) to the behavior inventory.
- Documented `tools/list_changed` notification; captured real error
  responses (unknown tool, bad argument) for the T-MCP-03 baseline.
- Aligned `tokenizers` to 0.22.2 (candle-core's version) to remove a
  duplicate from the dependency graph (593 -> 590 packages).
- Extended the native-code audit (F-05, model-license deferral).

## e75b963 (2026-09-16) — WP-00 baseline capture and dependency lock (S0)

- `baseline/lemma-0.21.0/`: live MCP wire capture (initialize, tools/list 29
  tools, tools/call), `upstream-lock.json`, provenance, behavior inventory,
  proposed `deviations.json`, test conventions.
- `tools/capture_lemma.mjs`: reproducible isolated-sandbox wire capture utility.
- `Cargo.toml`/`Cargo.lock`: resolved + compile-validated candidate dependency set
  (rmcp 3.4.0, lancedb 0.38.0, lance 11.0.0, fjall 3.1.10, candle 0.11.0,
  tokenizers 0.21.4; 593 packages).
- `baseline/lemma-0.21.0/dependency-native-audit.md`: native-code audit incl.
  lancedb 0.38.0 remote-feature compile constraint (F-01), candle conflict
  avoidance (F-02).
- `src/lib.rs` (library target); TODO/DONE/CHANGELOG trackers; AGENTS.md
  tracking rules; README S0 status.

## 9614885 (2026-09-16) — initial repository with implementation plans

- Added the reviewed implementation specification and test plan (`plans/`):
  design & concepts, implementation guide (WP-00…WP-13), quality/tests/benchmarks/conformance,
  plus `traceability.json` and `conformance_matrix.json`.
- Added `README.md` (overview, status, architecture, constraints) and `AGENTS.md`
  (agent working rules, plan authority, evidence discipline).
- Added dual license (`LICENSE-MIT`, `LICENSE-APACHE`), `rust-toolchain.toml` (1.96.0 baseline),
  Cargo skeleton, `.gitignore`, and IDE/MCP configs.
