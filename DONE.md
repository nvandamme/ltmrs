# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`.

---

## Unreleased Commit

### E5 wiring: provision + daemon enablement + live dense evidence (2026-09-27, uncommitted)

- New CLI `--provision-models` (parse/conflict/no-options/help tests):
  downloads + digest-verifies the 6 pinned E5 artifacts into
  `$HOME/.ltmrs/models`, idempotent (re-verifies, no re-download),
  10-min-per-file client timeout (default client has none — a stall
  would hang forever). Dispatch arm in `main.rs`.
- Daemon auto-detect enablement (`resolve_daemon_embedding`): full
  digest match → `E5SmallCached`; absent/partial/corrupt → `Disabled`
  + stderr diagnostic naming `--provision-models` (never half-enabled).
  `fetch_and_load` `dead_code` exemption removed (now live).
- Tests (all green): layout `models_path` pin; resolve Disabled on
  missing dir + on corrupt cache; lexical-fallback wire proof without
  models (`mode=lexical-fallback`, `dense_ready=false`); provision
  requires HOME. Full suite: `cargo fmt -- --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test`
  642 lib + 2 smoke, 0 failed (4 ignored: 3 pre-existing + 1 new
  live-E5 probe, env-gated `LTMRS_PROBE_MODELS`, skips without it).
- Live evidence (scratch HOME `/tmp/e5proof`, release binary):
  `--provision-models` exit 0, 6 files verified (model.safetensors
  470,641,600 bytes); rerun idempotent. stdio memory_add +
  semantic_search(explain:true) exit 0; `strace -f -e trace=%network`
  over serving shows 4 AF_UNIX sockets, ZERO AF_INET/AF_INET6.
- Live E5 probe (`--ignored live_e5_embed_against_provisioned_cache`,
  11.96s): query + passage vectors 384-dim, finite, L2-normalized,
  roles differ. Serving-time dense path (adapter + worker + sync
  bridge) proven independent of table contents.
- HONEST NEGATIVE: semantic_search over the provisioned daemon
  reports `lexical-fallback`, not hybrid — correct behavior, not a
  bug: the Lance table holds no dense rows (nothing runs the
  Projector with a real embedder; `E5SmallCached` docs defer the
  projection loop as "lands separately"), engine scores nothing,
  fallback serves the row and the explanation says so. Provision +
  enablement are done; end-to-end dense HITS need the projection
  write path (unscoped work package — proposal in session report).

### E5 end-to-end: projection drive → hybrid hits (2026-09-28, uncommitted)

- `Projector::project_pending` (one-shot drive: current generation +
  E5 fingerprint + injected embedder, returns resolved count).
- Reused the existing `impl Embedder for E5SmallAdapter` (Passage
  role, E5 chunk mapping, version stamp) — a first-cut sync-bridge
  `ServiceEmbedder` was DELETED after the nesting proof (block_on
  inside an async-driven projector panics; pure-sync adapter has no
  bridge at all). Plus `impl Embedder for Arc<Mutex<E5SmallAdapter>>`
  (per-tick clones without weight reloads; poison → lexical single
  unit via shared `single_chunk_unit` helper, no duplication).
- `Daemon::start_projection` loop (immediate first pass, then
  maintenance interval; `spawn_blocking` drive; per-tick generation
  freshness; failures loud + retry; parked without E5) wired into
  `serve()` + stdio `start_local_daemon`; aborted on shutdown.
- Tests: drive publishes/acks + failure leaves pending with lexical
  rows; worker parked without embedding; live ignored probe extended
  (wrapper vectors/chunks/version). Suite: 645 lib + 2 smoke green.
- LIVE hybrid proof (scratch HOME, release binary): add 2 memories →
  `projection converged 2 job(s)` → B-query (zero token overlap)
  returns entanglement 0.975 top with `mode=hybrid dense_ready=true`;
  A-query returns river-fox 0.975 top. Serving strace: 4 AF_UNIX,
  zero AF_INET. (Probe fixed mid-way: skipped interleaved
  `tools/list_changed` notifications — probe bug, not daemon bug.)
- Dual review findings fixed: (1) adapter load moved to
  `spawn_blocking` (was stalling I/O workers under the guard);
  (2) single-unit construction deduped into helper. Known limits
  (by design, not fixed): ~1GB RAM (query + projection adapters) +
  triple digest-hash at boot; no per-tick budget (bulk backfill =
  one long first tick); deterministic-oversize embeds retry silently
  each tick; fallback relevance can display >100% (pre-existing).

### Complete dual review vs all plans (2026-09-28) — VERDICT: compliant, no plan changes

- FORMAL (requirement-by-requirement trace, code re-read with
  file:line verification):
  - Part I §7 (CPU inference off Tokio workers): drive + adapter
    load on `spawn_blocking` ✓. §8.2 six projector steps untouched,
    only driven ✓. §8.3 freshness: immediate commit return (no
    mutation hook) ✓, `projection_lag`+`partial` wired live from
    `repo.projection_jobs()` (engine.rs readiness) ✓, no silent
    completeness ✓. §9: pinned recipe + digests reused for
    provisioning ✓, explicit online (CLI) vs offline-only daemon
    paths ✓, atomic tmp+rename writes ✓, E5 chunk mapping +
    offsets + `e5-chunks-v1` stamp via pre-existing impl ✓,
    oversized-errors-instead-of-truncating → SemanticPending ✓.
    §12 generation invalidation via publish guard + per-tick
    freshness ✓. Privacy: no content in diagnostics, local-only
    embedding ✓. Line 81 lexical-without-model path untouched ✓.
  - Part II: WP-05/06/07 boxes stay true (suite green); S4 core
    (pinned model + live hybrid + visible fallback) delivered;
    WP-13 offline-install task directly served. No requirement
    text changed → plans/ + traceability.json untouched
    (existing evidence intact, new evidence lives here).
  - Part III: executed (suite, live proofs with commands/outputs)
    vs not_run (ignored gate test, documented skip) discipline
    kept; no matrix claims made.
- CORRECTION to earlier claim: live `partial=true` is driven by
  the `!fts_ready` clause (FTS index never built in production —
  pre-existing unwired capability, explicitly displayed, out of
  scope), NOT proven to be the lag clause. lag→partial is
  unit-covered (engine tests) and code-verified; live attribution
  is impossible with the current wire (no `projection_lag` field).
- Steady-state verified: 330s idle watch → no convergence message
  (nothing pending; all cross-run jobs acked = crash-replay
  convergence live), search still hybrid/dense_ready.
  Multi-run "converged 2" counts reconciled as per-run first-pass
  convergence, not missing jobs.
- FUNCTIONAL second sweep: §9.2 auth/TLS (rustls HTTPS, public
  repo) ✓; provision re-verifies every file each run (both
  paths) ✓; shutdown aborts projection worker, no poison
  carryover (fresh adapter per start) ✓; tick panic → logged +
  loop continues ✓; 8-file manifest doc vs 6-file enforced cache:
  pooling/norm are Rust-implemented (recipe.rs), the 2 extra
  files are spec references — pre-existing doc precision, not
  changed (minimal-change rule).

### FTS branch: ensure + tick wiring + stale-snapshot fix (2026-09-28, uncommitted)

- Finding that started it: live `partial=true` traced to the
  `!fts_ready` clause — FTS never built in production (bench/tests
  only). Lance 0.38 probes (keeper tests): index covers
  post-creation rows (no rebuild policy needed); re-creation
  succeeds; handles are snapshot-pinned for index metadata
  (`fts_index_visible_across_handles_after_refresh` pins the
  refresh contract).
- `SearchTable::ensure_fts_index` (build-once, skip-when-ready) +
  tick wiring after successful drive (E5 mode only; Disabled
  parked, no behavior change). Bench already covers FTS-on
  (quality.rs builds + asserts `fts_query` hits) — no new bench;
  scalar BTree indexes noted out-of-scope (no readiness flag).
- REAL BUG FOUND by the branch: serving pinned a stale snapshot
  (tick-built index invisible without restart). Fix:
  `retrieve_sync` refreshes its cloned handle per request (zero
  struct changes; bench constructs `Engine` directly, unaffected;
  per-request reopen is ms vs seconds of inference). RED test
  `retrieve_sync_sees_post_construction_commits` failed correctly
  first. All production reads funnel through `retrieve_sync`
  (exec + recall_browse verified); embed fns touch no table.
- LIVE re-proof (release): `fts_ready=true partial=false` (first
  non-partial production search); B-query top entanglement 0.975,
  river-fox second at 0.511 (lexical leg now RRF-fused — designed
  behavior change, not a regression); idempotent skip confirmed
  (empty stderr, no rebuild). Serving stays offline (prior strace).
- Sweep finding fixed: provision digest-mismatch names the models
  dir removal remedy (fail-closed kept: no auto-redownload that
  could mask supply-chain tampering).
- Suite: 650 lib + 2 smoke green, fmt/clippy clean. Ledger note:
  DEV-001 already declares hybrid dense+lexical — this work makes
  production MATCH the ledger (previously bench/test-only true);
  no plan/ledger edits (no requirement changed).
