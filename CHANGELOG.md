# CHANGELOG.md

Work completed on previous commits, grouped by commit.
Content before `---` is instructions — do not modify. Add entries after the `---`.

---

## faaf731 (2026-09-28) — Final release evidence: quality, waves, tickers

### Inference parallelism probe: candle already threads (2026-09-28)

- Same 11-chunk probe: default threads 47s vs RAYON_NUM_THREADS=1
  298s (**6.3x from internal threading**; production runs show
  140-330% CPU). Candle-core depends on rayon itself — the pool
  is already scaled, not idle.
- Verdict: NO rayon/thread-pool added on top (would oversubscribe
  saturated cores; single bounded worker + sync adapters stay per
  RQ-22 and plan §7.3/§9). Bulk cost is inherent CPU inference
  (~4s/job measured end-to-end on release) + per-job commits.
- Fairness posture (unchanged): 100 jobs / 5-min tick bounds bulk
  CPU windows; interactive recall contends only transiently.
  Parallel bulk drive (batched forwards across cores) is a real
  future project, not a tweak — deferred deliberately.
- Temp probes deleted; no production code changed in this probe.

### BEIR harness built; full run needs GPU box (2026-09-28)

- Downloaded canonical SciFact (5183 docs / 300 test queries, UKP)
  and built the ignored eval harness (production-shape: real
  index + FTS + hybrid retrieve; nDCG@10/MAP/Recall/MRR).
- Proved working through 709 real indexed docs, then diagnosed a
  flat throughput wall (~4.2s/job: multi-chunk CPU forwards +
  per-job Lance commits) — the full run needs ~5h machine time.
  Per-phase probes + post-mortem job inspection done instead of
  guessing; added an idempotency regression test from the
  investigation (second drive resolves nothing).
- LM Studio note: BEIR correctly uses zero model-server traffic
  (local E5 embeddings); LM Studio served only the agent wave.
- Recorded honestly: harness done, full run open (GPU box);
  synthetic calibration stands as executed quality evidence.

### Live-agent wave via LM Studio qwen3.8-27b (2026-09-28)

- Hookup works: LM Studio server :1234 serves
  qwen3.8-27b-efficientthink-simpo-lynnstyle@q5, OpenAI-compatible,
  no key; 27B reasoning model answers in ~3s (reasoning_content
  separate; parse last content line). Harness:
  tools/agent_quality_wave.py (20 heldout cases, seed 7, temp 0,
  pinned prompt; verdicts YES/NO/UNCERTAIN verbatim).
- Run 1 (zero-shot rater): agreement 0.389. Analysis showed two
  confounds, not retrieval failure (exact-target hit@1 was 15/20):
  rater too strict on synonyms + template queries that don't
  answer their own targets (corpus validity gap — same human-
  review gap as the 300-case corpus).
- Run 2 (few-shot rater with synonym-bridging examples):
  agreement 0.632 (`reports/release-01/agent-wave.json`).
  Residual NOs mix genuine misses and corpus mismatches.
- Verdict: harness sound and reusable; numbers are wave evidence
  with stated caveats, not a quality gate (needs answerability-
  reviewed corpus + calibrated rater for gate use).

### Upstream harness exploration (2026-09-28)

- Mapped tmp/upstream-lemma/tests: ~40 node:test files (db, memory,
  guides, sessions, server, intelligence), manual MCP waves
  (mcp-smoke, recall-explain, c2/wave2/wave3/glm-agent quality),
  exactly ONE fixture file (unused-vec0.sql).
- RRF/MMR math parity verified in-suite (hand-computed fusion,
  missing legs, normalization, diversity tests both sides).
  Backup/migration/lifecycle areas have tests both sides.
- Nothing to import (their fixtures are thinner than ours:
  compat captures, reference vectors, calibration corpus).
- Live-agent waves recorded as not_run (need agents, like
  power-loss needs a lab). No code changed in this sweep.

### Clearing open tickers: soak/fault, corpus/calibration, differential (2026-09-28)

- Soak (new): deterministic 1000-op mixed schedule (seeded LCG,
  last-write-wins verified) + sustained racing-write consistency —
  both green, suite stays fast. WP-12 fault box updated (long-run
  soak + sustained-load fault stay honestly not_run).
- 300-case synthetic known-answer corpus
  (tools/gen_calibration_corpus.py → experiments/quality/
  retrieval-calibration.json): topic-disjoint dev/heldout,
  paraphrase queries (0.63 mean shared tokens). Calibration run
  (ignored, env-gated, 15 min): retention 1.0 through 0.70 on
  BOTH splits, collapses after — recommended 0.70, default HELD
  at 0.0 (thin margin + synthetic caveat; E5 similarities
  saturate high, ranking does the work). Changing the default
  needs owner approval. benchmarks.toml + WP-12 boxes updated.
- Differential: upstream 100-op sample fresh (26 ok / 74
  expected read-misses); ltmrs storage experiment re-run on the
  release tree (1614 ops 0 failures + 386 search ops 0 failures);
  transcripts in bundle. Full differential still tied to WP-12
  evidence (no contract delta).
- Validation: fmt/clippy clean, 664 lib + 2 smoke green.

### WP-12/13 ticker clearing: fault evidence + honest opens (2026-09-28)

- Fault box was stale ("no fault-injection harness"): injector
  exists with point tests (migration-atomicity, unknown-outcome,
  persist-barrier faults, all green). Added fragmented-state kill
  test (50 records + updates + feedback survive reopen) plus soak
  and sustained-load tests (see Clearing entry) and flipped the
  box to [x]-Partial with exact test names; long-run clock-time
  soak stays not_run (no harness exists).
- Re-audited every WP-12/WP-13 box against fresh evidence: all
  accurate except the fault box (fixed above). Calibration executed
  on the synthetic corpus (see Clearing entry; default held);
  human-reviewed corpus still open. Publish stays open (tag/push is
  owner action — deliberately untagged on a moving tree).
- Validation: fmt/clippy clean, 664 lib + 2 smoke green (7 ignored:
  export/storage/load reference runs + live-E5 + calibration + BEIR
  + batch-parity — all env-gated or harness-runners).


### Load + timing differentials (2026-09-28)

- Load harness re-run on release tree (ignored
  load_experiment_reference_run): closed 811 ops 0 failures
  (gets p50 3µs, puts p50 16.3ms — durability barrier cost);
  open Poisson-200/s 811 ops 0 failures, tight tails (puts p99
  11.3ms). Summaries in bundle; wp12-load-01 evidence restored
  untouched.
- Timing differential vs upstream (salted 100-op stream, same
  box): puts med 4.4→1.2ms, search med 1.2→0.2ms, get-miss
  0.6→0.1ms. First run invalid (upstream dedup refused 52/53)
  and redone via sanctioned per-seq resalting, not averaged.
  Small stores, lexical paths, indicative only. Runner
  (tools/bench_against_ltmrs.py, transcript-compatible) +
  transcripts in bundle. Get-ok counts incomparable by contract;
  our duplicate gate passing salted content is DEV-007, known.

## 9328347 (2026-10-03) — P1/P2 review fixes, release evidence, dependency majors

### P1 code-review fixes: shared daemon, durable sessions, atomic guides

- P1-1 shared daemon (`runtime.rs`, `server.rs`, `frontend/serve.rs`):
  listener in `Arc`, `spawn_socket_server` background accept loop,
  `connect_or_spawn` + `connect_with_retry` (bounded ~1s) on lock race,
  owner lingers serving socket peers after own stdio closes (verified:
  `waiting(mut self)` drops our bridge first, so the linger branch
  counts only real peers; 60s bound, never a hang).
  Tests: socket second handshake, attach-to-owner, lock-race fallback.
- P1-2 durable sessions (`registry.rs`, `dispatcher.rs`, `tools.rs`):
  `record_attempt` dedups deterministic UUIDv5 IDs, `has_attempt` guard,
  `Dispatcher::persist_sessions` eager persist before every ack
  (attempt/end/start/guide-track), counters increment exactly once.
  Tests: dispatcher replay-once, kill-without-shutdown survives,
  tool replay-once (counters).
- P1-3 atomic guides (`service/repository.rs`, `daemon/tools.rs`):
  `rename/remove_guide_references` fresh-read patch (content preserved),
  `merge_guides_atomically` single-tx (refs+deletes+put, no tear),
  `practice_guide_idempotent` op-logged single-tx (no double-count),
  rename/forget/merge propagate errors (no `let _`).
  Tests: rename-preserves-content, merge-all-or-nothing,
  practice-replay-once.
- Reviews: formal (coverage vs P1/P2 goals, no plan change) +
  functional (linger liveness proven via rmcp `waiting(mut self)`
  + smoke timing; tx iteration-then-mutate ordering; Notify
  wake-then-wait permit semantics). No findings outstanding.

### Deps + P2s: pruning, updates, projection wake-up, release evidence

- Deps (`Cargo.toml`, `Cargo.lock`): removed 3 unused direct deps —
  `anyhow` (0 uses), `indexmap` (0 uses; `serde_json/preserve_order`
  already covers wire order), `candle-transformers` (0 uses; E5 runs
  the local `bert_impl`). Targeted updates: fjall 3.1.10→3.1.11,
  rmcp 3.4.0→3.5.0, thiserror 2.0.20→2.0.21, uuid 1.26.1→1.27.0,
  libc 0.2.189→0.2.190. `cargo check` + full suite green after both
  steps. Frozen `baseline/.../dependency-native-audit.md`
  (2026-09-16) still lists candle-transformers — stale by design,
  not edited.
- Toolchain fallout (pre-existing `rust-toolchain.toml` 1.96→1.99 bump):
  reinstalled stale clippy/rustfmt components; fixed 3 new
  `needless_borrows_for_generic_args` lints in `skills/installer.rs`
  (shared closure → per-site mapping; same behavior).
- P2-1 projection wake-up (`service/repository.rs`, `daemon/server.rs`,
  README): `set_commit_hook` fires once per committed mutation (never
  on replay; panic-safe); `ProjectionTrigger` (Notify + interval
  select); `start_projection` waits on wake-or-interval instead of
  bare sleep. Readiness signal already existed (`projection_lag` /
  `partial`); README latency paragraph updated.
  Tests (paused-clock, deterministic): commit-hook-once-not-replay,
  trigger-wakes-on-commit-not-interval.
- P2-2 release evidence (`tools/gen_release_evidence.sh`,
  `tests/release_evidence.rs`): regenerable sanitized bundle
  `reports/release-<short-sha>/` (manifest.json with HEAD, toolchain,
  lock digest, direct dep versions, fmt/clippy/suite status; no memory
  contents, secrets or absolute paths) + acceptance test in quick
  mode. Regenerate after commit — bundles name HEAD but run on the
  committing tree.

### Major upgrades: latest majors where the graph stays single-version

- Taken: sha2 0.10→0.11 (4 `{:x}`-on-digest sites → byte-iterating hex;
  0.11 digest output dropped `LowerHex`), getrandom 0.3→0.4 (no call
  change), lance 11→12 + lance-index 12 + lancedb 0.38→0.39 (zero code
  changes; storage/search suites green).
- Reverted with evidence: arrow 58→60 compiles our code but forks
  arrow_array/arrow_schema against `lancedb::arrow` (lance 12, lancedb
  0.39 and datafusion 54.1 all resolve arrow 58) — back to 58 with a
  Cargo.toml comment pinning the ecosystem line.
- Deferred with reasons: reqwest 0.13 (would fork vs lancedb `remote`
  on 0.12; ours stays single at 0.12.28), tokenizers 1.0-rc.2
  (pre-release; candle-core 0.11 needs 0.22 API — upgrading creates
  the dup it was pinned to avoid), libc 1.0-alpha.5 (alpha).
- Result: every direct dep resolves to exactly one version
  (rmcp 3.5, lancedb 0.39, lance 12, fjall 3.1.11, candle 0.11,
  reqwest 0.12.28, sha2 0.11, arrow 58, rest current). Remaining
  multi-version crates are all transitive-ecosystem and unactionable
  from our manifest: lancedb's own sha2 0.10, ring's getrandom 0.2,
  ahash/lsm-tree getrandom 0.3, lance-12-vs-datafusion-54 splits
  (object_store 0.13/0.14, axum 0.7/0.8, rand, itertools), snafu/
  darling/syn/hashbrown lines.
- Validation: `cargo fmt --check`, `cargo clippy --all-targets
  -- -D warnings`, `cargo test` (675 lib passed, 0 failed, 7 ignored;
  + release_evidence + 2 smoke). No plan change.

## 538c0b2 (2026-09-29) — Release build profile + publish record

### Release build profile: LTO thin (2026-09-29)

- Neither profile set LTO (Cargo defaults). Set `lto = "thin"`
  for dev + release (owner-ordered): dev full build now 6m40s
  (link cost — reversibly expensive for iteration), suite green
  (664+2) under the new profile, release binary serves
  (initialize + clean exit).
- Measured effect: binary grew 286.1MB → 290.0MB (+1.3%, thin
  LTO trades size for speed); runtime delta unmeasured. Keep or
  revert on owner call.

### Publish v0.1-alpha (2026-09-29)

- Windows x64 blocked with full analysis: Unix-only IPC
  (tokio UnixStream gated out on Windows, std::os::unix uses,
  RQ-20 peer-cred design) + no Windows machine to verify on.
  Cross-toolchain assembled (Arch GCC 16.2) but correctly unused
  — shipping an unverifiable binary would violate evidence
  discipline. Port is future work, not a review fix.
- Tag disputed honestly: package reports 0.1.0-alpha and §2.3
  reserves v0.1; owner chose v0.1-alpha. Pushed master
  fast-forward (no force) + tag; GitHub release carries the
  Linux x86_64 binary + SHA256SUMS with known limitations in
  the notes (no silent claims).
- LTO rebuild supersedes those artifacts (same tag name, new
  binary + sums re-uploaded; tag moved, never duplicated).

### Publish executed: tag moved, release live (2026-09-29)

- Amended finale (message fix) as 538c0b2 with corrected body;
  baked SHA worktree-only and stopped — no amend loop.
- Remote tag was dangling at an unpublished hash (own earlier
  truncated push output hid a tag push): recovered with
  delete + recreate, never force. No force-push used anywhere.
- Master pushed fast-forward (de8139c..538c0b2); tag v0.1-alpha
  now points at the release commit; Linux x86_64 binary +
  SHA256SUMS (re)attached; draft flipped live. Verified: tag
  target, both assets, published timestamp.

## 1ef65d1 (2026-09-28) — Batch embedding per memory (bulk-drive Phase 1)

### Batch embedding per memory (bulk-drive Phase 1) (2026-09-28)

- `Embedder::embed_texts` with order/error-preserving default +
  `render_chunk_rows` routed through one batch call (row shape
  unchanged; failures stay lexical-only). E5 override does a single
  `embed_batch` (Passage role) with defensive length match; shared
  `Arc<Mutex>` handle forwards so the live tick path actually uses
  it (caught in review before merge).
- Delivered subagent-driven (implementer per task + reviewer per
  task + whole-branch review, all clean; one fix round split a
  stray test out of scope). Phase 2 parked by measurement
  (candle scales 6.3x internally; data-parallel forwards would
  time-slice the same cores).
- Tests: batch-default mapping, parity within 1e-5 (direct +
  shared-handle, weight-gated), full suite green.

## 0c01e55 (2026-09-28) — Release closure: version, docs, approvals, export route, readme completion

### Visualizer /api/export route (upstream parity) (2026-09-28)

- Upstream README+source audit (after a false "nothing to compare"
  claim — the full tree sits at tmp/upstream-lemma/): skill surface
  reproduced completely (no serving API exists upstream); the one
  real gap was `GET /api/export` (JSONL attachment). WP-10's
  "claimed routes" never normatively scoped this out (the
  narrowing lived only in an implementer evidence note).
- Implemented TDD-first from the upstream contract read directly
  (token via ?token= — window.open can't set headers; one JSON
  object per fragment; attachment filename memory-export.jsonl):
  `CanonicalExport::to_jsonlines`, `respond_extra` helper
  (existing `respond` byte-identical), route branch, index link,
  module docs. Auth matrix matches house rules (403, POST→405).
- Tests RED-first: route (denied/wrong-method/200+headers+row),
  empty-body edge. Full suite 659+2 green; live curl proof
  (403 bare, 200 + headers + full memory object with token).
- Known nuance (kept, not fixed): our objects serialize source
  PascalCase (`Ai`) from derived serde; changing it would break
  backup back-compat. Shape parity is what the feature needs.

### README completion vs upstream lemma (2026-09-28)

- Upstream source found at tmp/upstream-lemma/ (was checking the
  thin baseline only — mea culpa recorded). Diffed its README
  section by section.
- Added: Quick Start (generic stdio host config), How It Works
  (prefetch/preload, pipeline, types, redaction, explain),
  Tools (29, by family + backup user-flow), Security
  (local-first, redaction, loopback+token, unencrypted backups).
  Fixed stale "approvals pending" line. Every claim verified
  against code/tests/runs first (incl. confirming backups are
  unencrypted and the token/403 behavior).
- Skills templates: reproduced (install/idempotent/version-aware/
  atomic/foreign-refusal + shim with collision detection, WP-10
  pinned). No serving API exists upstream to reproduce.
- Follow-up correction: the `/api/export` endpoint WAS subsequently
  built (see Export entry) after establishing WP-10 never
  normatively scoped it out — the narrowing lived only in an
  implementer evidence note.

### Remnant hunt across plans (2026-09-28)

- Hygiene: no DBG/TODO markers; no secrets/weights in history
  (largest tracked file is a 585KB test fixture); worktree steady.
- Version bump is restore-safe: `ltmrs_version` is metadata-only
  (verify checks format/digests, never version equality).
- Catalogued pre-existing (not built — new features, not
  remnants): `health_report` dead API (tested builder, no
  trigger); `expire_leases` unwired; sessions.json grows ~per
  connection (harmless since live-count fix); scalar BTree
  indexes unwired (no readiness flag drives them; filtered
  queries correct, just unindexed); socket-mode E5 (no config
  producer — stdio-only dense stands).
- Nothing of mine left unfixed; no code changed in this sweep.

### Deviation approvals + 008/009 follow-ups (2026-09-28)

- Approvals flipped to `approved by owner 2026-09-28` for all 12
  (001/002/003/005/006/007/010/011/012 first, then 004/008/009).
- DEV-008 answer: baseline captures mechanisms only (seed list in
  prompt; 4 seeds; coaching blocks; tech autodetect; distill
  suggestion) — no content/heuristics. Filed as TODO backlog
  item (not a plan WP: host/model concern, not canonical contract).
- DEV-009 answer: mechanism verified NOW (smoke + live probes to
  the process boundary); per-host uptake is FUTURE (needs real
  hosts; unobservable from inside). Documentation approved as
  accurate.
- Validation: fmt/clippy clean, 657 lib + 2 smoke green.
- Approvals: all 12 deviations now `approved by owner 2026-09-28`.

### Deviation fix investigation + release dual review (2026-09-28)

- All 12 deviations investigated for FIXES (not just wording):
  intentional-and-kept: 001/005 (core retrieval), 002 (hardening),
  003 (correctness; keep-open preserves binding), 006
  (architecturally impossible), 007 (determinism preferred; 0.80
  verified at tools.rs:1084,1349), 008 (architecture; no-seed
  verified), 010 (refusal IS RQ-21 compliance), 011/012 (core
  features, now production-true).
- DEV-004 built (see Provenance entry, 7213efa): expanded corpus
  implemented + tested; ledger narrowed.
- DEV-009 partially verifiable only: mcp-stdio to the process
  boundary is proven (smoke + live probes); host-side uptake is
  unobservable from inside. Ledger already says exactly this.
- Wording fixed in ledger (2 lines, JSON valid): `calibrated`
  → normalized + AD-05-open in DEV-001/005. Approvals complete
  (owner; see Approvals entry above).
- Release dual review closed: socket-mode E5 unwired (no config
  producer — stdio-only dense, documented); serve() wiring is
  consistent future-proofing; backup excludes weights by
  construction; restore re-enqueues jobs; conformance matrix has
  no retrieval rows; absolute-claims sweep clean; Cargo/native
  policy intact (no new deps); version bump safe (skill versions
  independent, suite green).

### WP-13 release tickers: suites, offline, rollback, matrices, ledger, archive, version (2026-09-28)

- Suites: `cargo test --workspace --all-targets` 655 lib + 2 smoke,
  0 failed (4 ignored); fmt/clippy clean. Compat fixtures run
  in-suite; upstream-differential bench not re-run (no contract
  change; upstream server not provisioned — honest not_run).
- Offline (ENFORCED via `unshare -Unr`, not just absence-of-use):
  provision-in-netns control fails loudly exit 1; full serving flow
  (add/search/read, E5 provisioned) exit 0 with zero routes.
  Reqwest audit: only the provision CLI builds network clients.
- Migration: unit suites green; legacy refusal tested; rollback
  instructions now exist (README Operations) and were executed
  live (see Restore entry, ba54d03, for the full cycle; blockers
  found there are fixed, not waived).
- Matrices: `reports/release-01/matrices.md` (tool 117/115 cells
  referenced, host, model, OS, durability — executed or not_run).
- Deviations: all 12 reviewed claim-by-claim (0.80 threshold,
  passthrough, no-seed verified); fixed `calibrated` → normalized
  (AD-05 open) in DEV-001/005; all approved (see Approvals entry
  above).
- Archive: `reports/release-01/` (INDEX, sbom 594 comp — no
  GPL/AGPL/proprietary; binary/lock/manifest/fixture digests;
  rollback transcript; suite record). Version: `0.1.0-alpha`
  (§2.3; skill versions independent, suite green, binary reports
  it). Publish (tag/push) needs owner approval — NOT done.

### Operator docs: E5 + Operations README sections (2026-09-28)

- Codebase scan for implemented-but-undocumented operator surface
  found: E5/provision behavior, managed-home layout, full CLI
  command map, exit codes, visualizer token auth, backup/restore
  shape, no-config-file rule. None were in README (limits lived
  only in CHANGELOG history).
- Added `## Dense retrieval (E5)` (provision, enablement,
  async-indexing contract, costs, edge semantics) and
  `## Operations` (layout table, command map with `--help`
  authoritative, exit codes, token URL/port/403, backup tool
  chain). Every sentence verified against mechanism, test, or
  measured run before writing.
- Health/doctor report is intentionally NOT documented:
  `health_report` has no CLI or MCP trigger (dead-ish API) —
  documenting it would be a false claim.

## 7213efa (2026-09-28) — Provenance vocabulary (DEV-004)

### Provenance vocabulary (DEV-004, owner-agreed) (2026-09-28)

- `MemorySource` expanded beyond user|ai: user|ai|web|paper|book|code
  with disjoint documented rules (URL-captured vs DOI/arXiv-identified
  vs ISBN-identified vs path-identified; verification state stays out
  — feedback counts already model it). Ingress (tools.rs parse+coerce),
  stats (dynamic by_source), icons (non-ai → person, upstream parity),
  serde round-trip all covered; truly-unknown still coerces to ai
  (tested residual, e.g. 'user-corrected formal review').
- Tests: provenance_vocabulary_round_trips (RED-first) +
  memory_stats_groups_expanded_provenance. Ledger narrowed
  (behavior, impact, test, release_wording); owner-approved.

## ba54d03 (2026-09-28) — Restore session survival + live readiness

### Restore session survival + live readiness (2026-09-28)

- Two release-blockers found live and fixed TDD-first. (1) Post-
  restore sessions bricked silently: the daemon closed on
  GenerationMismatch while the bridged frontend dialed the
  bound-but-unaccepted socket (read pends forever). Fix:
  `IpcClient` tracks bridged mode (`connect()` no-ops on a live
  bridge, loud `NotConnected` without one); daemon keeps the
  connection open on mismatch and accepts same-identity epoch
  refresh on authenticated connections (different IDs stay a
  violation — RQ-05 preserved); stale path uses epoch-only forget
  (no pointless redial, fatal on bridge). (2) Restore blocked
  after any restart: readiness counted persisted channel history.
  Fix: RAII live-connection counter (`note_live_connect`/
  `disconnect`, saturating floor); preview + confirm use it.
- Tests (RED observed each): bridged-connect never dials; same-
  connection mismatch retry; timeout-guarded full-stack generation
  bump; dead channels → READY; live count resets on registry load.
  Full suite green; live rollback cycle verified same-session
  (backup/restore/rollback with state assertions, exit 0).

## 6c16a54 (2026-09-28) — Release review: per-tick projection cap

### Release review: per-tick projection cap (2026-09-28)

- Release-grade review vs all plans surfaced a §7.3 fairness gap:
  the drive drained ALL pending jobs per tick (unbounded bulk
  backfill). Fix: `run_limited`/`run_capped` (unbounded
  `run_until_idle` preserved for bench/tests), `project_pending`
  takes `max_jobs`, daemon constant 100/tick with rationale;
  remainder converges over successive ticks with per-tick logs.
  RED cap test first (signature-missing, then remainder-converges
  asserted).
- Adjacent audit results (verified, no changes): socket-mode E5 has
  no config producer (stdio-only dense today; `serve()` wiring is
  future-proofing consistent with `start_maintenance`); backup
  exports the canonical repo only (models/ cannot bloat backups);
  restore re-enqueues projection jobs (worker converges them);
  conformance matrix has no retrieval rows to invalidate; no new
  MCP tools (count intact); provision digest-mismatch fails closed
  (no auto-redownload masking tampering) + removal remedy named.
- AD-06 stays OPEN (plan-level decision; FTS probes are evidence
  toward it, null-vector half not probed here) — no plan/ledger
  edits. Absolute-claims sweep of all new strings: every
  never/always/every verified against mechanism + test.
- Validation: `cargo fmt -- --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test`
  651 lib + 2 smoke green. Release boot smoke (warm page cache):
  initialize in 3s, immediate hybrid + dense + fts,
  partial=false, top entanglement 0.975, empty stderr.

## 90a0d34 (2026-09-28) — E5 end-to-end: provision, projection drive, FTS ensure

### E5 wiring: provision + daemon enablement + live dense evidence (2026-09-27)

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

### E5 end-to-end: projection drive → hybrid hits (2026-09-28)

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

### FTS branch: ensure + tick wiring + stale-snapshot fix (2026-09-28)

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

## fa471e9 (2026-09-28) — WP-13 release evidence and agent process docs

### WP-13 release evidence: suite refresh + native-code policy (2026-09-27)

- Full validation green on the collapsed tree: `cargo fmt -- --check`
  clean, `cargo clippy --all-targets -- -D warnings` clean,
  `cargo test` 635 lib + 2 smoke, 0 failed.
- Release build green (25.77s); binary sha256
  `b82f158a73985804d464cec583a4e2970e6b815276e24f480fe3293916044189`
  (285,312,656 bytes). Digest differs from the pre-collapse build
  (new code since) — this is the current release-candidate digest.
- Native-code policy re-check: dependency set matches WP-00 audit
  exactly plus `getrandom 0.3` only; its build script probes the rustc
  version (no C compilation hooks); runtime is the `getrandom(2)`
  syscall via already-accepted `libc`. No new native code.
  Policy ACCEPTABLE, unchanged.
- Offline proof (lexical surface, fresh HOME, release binary):
  initialize + memory_add + semantic_search (hybrid:false) +
  memory_read all succeed with clean exit 0; data persists across
  runs in the managed store. `strace -f -e trace=%network` over the
  full run shows exactly ONE socket (the daemon AF_UNIX socket;
  SO_PEERCRED same-user check) and ZERO AF_INET/AF_INET6 uses —
  no TCP/UDP/DNS of any kind. Caveats: network was available but
  unused (absence-of-use proof; namespace isolation unavailable in
  this environment); covers embeddings-disabled production
  configuration only; batch-mode stdin-EOF can race a queued
  response (write landed, response lost — observed once, not
  adjudicated).

### Agent process docs (AGENTS.md, no DONE entry)

- New `Module Organization and Code Hygiene` section (STRICT,
  repo-wide, permanent): domain modules own their features, split
  by intent, no duplication.
- New `Commit Workflow for CHANGELOG.md` section: date-heading
  prepare/verify/commit/hash/worktree-header/stop, no-amend rule,
  hard constraint against code/changelog separation.

## 4714b30 (2026-09-27) — WP-13 review fix pass over WP-12

### WP-13 review fix pass (retrieval, surface, durability)

- Full formal + functional reviews across all plans (3 parallel slices:
  retrieval+search, surface, durability+plans-consistency) returned NOT
  READY x3; every Critical/Important finding fixed TDD-first (RED observed),
  cheap Minors folded in, 3 deferred with rationale.
- Critical: established frontend connections bricked after restore (stale
  epoch, no re-handshake path). New `roundtrip_with_rehandshake` (forget
  epoch, re-handshake adopting live generation, single retry with fresh op
  id); `ensure_handshaked` connects when needed.
  (`established_connection_rehandshakes_on_stale_generation`)
- Important: no-answer degraded-leg parity (`dense_failed` threaded through,
  2 tests); visualizer token now 128-bit getrandom 0.3 (was fixed-key
  DefaultHasher over pid/time/counter); reference oracle parity restored
  (absolute-only revision advance, non-Live reject, relate id-reuse,
  merge alias check+register; 4 oracle tests); relation ids bound to full
  input (note/created_at); typed `DomainErrorCode::Contention` with
  handshake retry + Busy/daemon_busy wire mapping; N10 stale pins fail
  loudly; I3 confidence column + source pre-filter + gate; plan/ledger
  drift fixed (02 min_similarity, RQ-04/06/13/18/19 evidence, DEV-011/012,
  README absolutes incl. decided AD-01 Option B).
- Minor: RESTOREDBG print, e5 dead branch, chunker_version sql_quote,
  strict confirm/symbol parsers, bert zero-dim validation, context comment
  contract, NaN min_confidence rejected at resolve, refcounted quota slots,
  loud registry persist failures, ANN-caveat note. Deferred: restore-test
  5ms margin, barrier Validation code, frontend Busy-retry.
- Queue batches under review: MMR mean vectors, named-column resolution,
  DomainResult rows, dense IS NOT NULL leg, E5 chunking bridge, strict
  parsers + allowlist, project normalization, registry tmp+rename persist,
  hybrid Option + explain, per-boot token auth, session-link single apply,
  atomic restore_replace (9 keyspaces), preview TTL + lease re-check,
  safety backup/rollback, persist barriers, namespace atomicity.
- Re-review (second full pass, same 3 slices): retrieval READY; surface +
  durability NOT READY — all fixed TDD-first, no new deferrals.
  - Channel binding: frames were frontend-bound only; same-frontend
    cross-channel frames executed in sibling sessions (RQ-05). Handshake
    channel now captured, mismatches refused (`channel_mismatch`).
    (`channel_spoofed_frames_are_rejected`, raw socket pair)
  - Sticky dead streams x2: generic handshake rejection kept a
    dead-but-connected stream; transport failures never forgot the epoch.
    Both now close/forget; the next call reconnects.
    (`rejected_handshake_closes_dead_stream` via fake rejecting server,
    `transport_failure_forgets_handshake` via dead-pair swap)
  - GC + counters: `gc_expired` and `mark_build_dirty` collect-then-write;
    multi-receipt GC test, corrupt-epoch-bytes test, restore unwritten
    categories report 0 with reason, `client_count` sums holders,
    `explain_search` reports the effective mode (hybrid iff dense ran)
    with honest fallback flags.
  - Retrieval minors: confidence/chunker gate tests split + named,
    genuine complete-no-answer pinned, direct/list stale-pin tests,
    tiny-budget bare-cut test, transitive out-of-scope bundle test with
    documented scope-purity tradeoff.
  - Tracker hygiene: `deviations.json` + `traceability.json` restored to
    native 2-space indent (diffs purely additive); WP-00 entries merged
    under `edd09a7` (history squash proof); all 31 headers resolve in
    history.
- Validation: `cargo fmt -- --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test` (600 lib + 2 smoke, 0 failed),
  `cargo build --release` (45s; sha256
  9f0b384d5e3e5ddfd3eec99824f1c3d92277a4799823957652eb46daa88918bb;
  getrandom native-code policy ACCEPTABLE). NO RELEASE yet (evidence
  remainder: offline install, migration/rollback docs, matrices, SBOM,
  version labeling).

## 75169fe (2026-09-27) — WP-12 bench harness, first runs, lemma comparison, quality legs

### WP-12 bench harness + first runs + lemma comparison + quality legs

- Harness `src/bench/` (lib `bench`): seeded xorshift generator + op mix,
  unit-normalized fixed probe vectors, JSONL history recorder with readback,
  exact nearest-rank histograms, config digests, hardware provenance
  (HARNESS_VERSION 2; tail percentiles carry indicative-only notes below
  TAIL_SAMPLE_FLOOR). No new dependencies.
- Storage experiment: put/get driver + async FTS leg (embedding-free rows,
  index-presence guard, probe identity preserved), pure summarize.
  Reference run wp12-storage-01 (seed 7, 2000 ops) for real: fjall put p50
  80us/p99 148us, get p50 32us/p99 70us; lance FTS (500 rows) p50 6.3ms;
  0 failures. Evidence in reports/ (gitignored), spec in benchmarks.toml.
- Load runners: closed-loop (scoped threads) + open-loop (paced scheduler,
  per-op queue from scheduled arrival, no shedding) sharing one executor
  (payloads built pre-timer). Reference run wp12-load-01 (seed 11, 1000
  ops): closed 4 workers, open Poisson 200/s, queue p99 157us, 0 failures.
  Telemetry observes store bytes (Fjall 64MB preallocation absorbs unit
  runs); projection/maintenance honestly not-applicable.
- Lemma comparison wp12-lemma-01 (user-requested, twice): identical 2000-op
  stream through pinned upstream 0.21.0 via tools/bench_against_lemma.mjs
  (HOME-isolated, timeouts, arg validation). Verdicts: put/search NOT
  COMPARABLE (dedup-gate refusal cascade diverged the corpus); get
  miss-latency shape only (811us vs 32us p50, error-vs-ok semantics differ).
- Quality: label schema + split-integrity validator + pure IR/safety metrics
  + ablation runner; 10-case safety fixture passes through the lexical leg
  (8), the deterministic engine leg (7, non-vacuous obsolete exclusion) and
  the real-E5 leg (French cross-lang retrieves English doc, 11s,
  skip-guarded). Correction: Lance is the vector table, E5 is the embedding
  engine; relational judgment needs no model.
- Reviews: formal (plans-coverage) + functional (bug hunt) passes; findings
  fixed (load-summary failure derivation, probe identity, timer placement,
  release-mode asserts, fresh-dir reference runs, driver timeouts/cleanup,
  E5 single load, tail notes, not_run ledger, harness version).
- Explicit not_run (benchmarks.toml [[not_run]]): fault/soak, 300-case
  corpus, 4+ ablation legs, ANN, dev calibration, contended writes,
  search-load legs, lemma unique-content variant.
- Validation: `cargo fmt -- --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test` (524 lib passed + 3 ignored reference runs + 2
  smoke, 0 failed).

## 435035e (2026-09-26) — WP-11 native backup restore and legacy verdict

### WP-11 native backup/restore + legacy verdict

- Format `ltmrs-backup` v1 (`src/interchange/backup.rs`): coherent
  single-read-tx full export (memories/relations/guides/feedback/
  suggestions; sessions ride from the registry; archives/history/projects
  have no storage and count zero by design), manifest counts + snapshot
  digest, tmp+rename atomic publish, re-read verify, 128 MiB bound.
  `backup_create` native MCP tool (explicit directory, no invented
  default); tools/list 29 (26 frozen-verbatim + 3 native).
- Restore machine (`src/interchange/restore.rs`): preview readiness +
  blockers, single-use 10-min TTL tokens bound to digest+generation+
  channel; confirm re-validates everything; safety backup first;
  single-tx atomic replace with quarantine list; generation bump
  retiring pre-restore pipelines; live sessions abandoned (reported);
  rollback = second restore of the safety file. `backup_preview` /
  `backup_restore` native tools; end-to-end rollback test.
- Legacy verdict (`src/interchange/legacy.rs`): `.lemma-backup` SQLite
  payloads refused explicitly (RQ-21, no C engine) with reason;
  never converted, never relabeled; no-mutation hash proof; unknown
   top-level keys flatten-captured, counted and reported through
   backup_preview (pre-confirm warning + structured count) and
   backup_restore (report text + structured; tool descriptions document
   the loss fields).
- Hardening: truncation/depth-bomb/lying-manifest/oversize tests;
  concurrent-export consistency; pipeline invalidation proof
  (stage → restore → publish refused via generation gate).
- Maintenance barrier by construction (atomic replace + optimistic
  conflict failure + generation retirement + projector publish gate
  checking generation, revision and recallability).
- Reviews: formal (seam inventory, role separation, token lifecycle) +
  functional (#[test] theft repair, DomainError has no Display,
  namespace issuance in tests, export_snapshot partial → export_full,
  virtual-session counts, backtick shell mangling).
- Validation: `cargo fmt -- --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test`:
  492 lib + stdio_smoke (1) + vis_smoke (1) passed, 0 failed.

## d263d8e (2026-09-26) — WP-09 dense guide candidates and differential workflow replay

### WP-09 remainings: dense guide candidates + differential workflow replay

- Dense guide-catalog leg (`QueryEmbedder::embed_passages` defaulted trait
  method; `ServiceQueryEmbedder` Passage-role batch override over the worker;
  `SearchBackend::embed_query_sync`/`embed_passages_sync` bridges;
  `guide_catalog_text` + `suggest_guides_dense` + wiring in `exec_guide_get`):
  cosine-ranked (ranking::cosine, >0.0 only) catalog guides append after the
  token matches (cap 5); any failure degrades to byte-identical token output;
  scores never displayed. Role separation enforced by types (passages never
  through the Query seam).
- Differential workflow replay (8-step recall -> act -> persist):
  `tools/capture_workflow.mjs` (synthetic fixture script over MCP stdio) +
  `tools/normalize_workflow.mjs` (placeholder normalization, tracked, byte-
  reproduces the fixture) + `tests/compat/lemma_0_21_0/workflow_fixture.json`
  (pinned upstream transcript + provenance + regen commands) +
  `differential_workflow_replay_matches_upstream` (data-driven replay, exact
  parity on adds/guides, structural sets/deltas with declared divergences:
  4 seed fragments, coaching blocks, read ordering, auto-detected techs,
  distill suggestion, cwd-derived projects).
- Notable upstream behaviors observed (recorded, not replicated): fuzzy
  duplicate refusal on near-identical adds; auto-link on add; seed catalog;
  duplicate guide entry quirk.
- Reviews: formal (data-driven replay args; tracked normalizer; byte-exact
  fixture reproduction) + functional (regex $$ replacements; frozen-clock
  1970 dates; (global) key pass-through; spawn_blocking for block_on;
  type_complexity alias; GuideCreateArgs has no Default).
- Validation: `cargo fmt -- --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test`:
  476 lib + stdio_smoke (1) + vis_smoke (1) passed, 0 failed.


## 9815501 (2026-09-26) — WP-10 CLI skills stdio visualizer hosts shim and server names

### WP-10b managed skill installer

- `src/skills/installer.rs` (new) + `ltmrs_skill.md` (native asset):
  ownership markers (version + sha256), decision table (fresh install,
  idempotent reinstall, outdated update, modified/foreign refusal,
  explicit override), atomic temp+rename writes, numeric version order.
- `src/cli.rs` + `main.rs`: `--install-skill` wired (explicit home,
  exit 0 installed/current, exit 1 refusals with backup-first advice).
- Reviews (formal + functional): fixed spoofed-marker bypass (dotted-
  numeric validation), empty-content idempotency, numeric-order coverage,
  root-proof denial test, distinct refusal messages, version-bump rule.
- Tests: 7 decision branches + 3 hardening (spoof, empty, numeric) +
  2 CLI command pins.
  (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`: 447 passed, 0 failed.)
- Notes: legacy/upstream skill content NOT snapshotted (no mislabeling);
  explicit legacy path + host recipes remain (WP-10c/hosts).

### WP-10c stdio serving + daemon lifecycle

- `src/frontend/serve.rs` (new): `resolve_home` (missing/empty HOME is a
  runtime error, never an invented store), `stdio_layout` pins
  `$HOME/.ltmrs/{store,sessions.json,search}` + runtime base,
  `start_local_daemon` (in-process `Daemon::start` + `UnixStream` pair +
  spawned `handle_connection`; caller holds the daemon for the singleton
  lock, `shutdown` persists sessions), `connect_remote` (fail-fast connect,
  handshake stays lazy so the snapshot prefetch runs in both modes),
  `serve_stdio` (rmcp `serve_server` over `transport::stdio`, `QuitReason`
  mapped, daemon shut down on every exit path).
- `src/main.rs`: `#[tokio::main]`; `Stdio` arm serves (exit 0 on clean stdin
  EOF, 1 on runtime failure). `Cargo.toml`: rmcp `transport-io` + tokio
  `io-std` (pure Rust, no new native deps).
- `tests/stdio_smoke.rs` (new): live binary over piped stdio — `initialize`
  returns `serverInfo.name == "ltmrs"`, stdin EOF exits 0, managed store
  created under isolated HOME.
- Reviews: formal (socket-mode prefetch loss → lazy handshake; init-failure
  shutdown skip → shutdown-on-every-path) + functional (`QuitReason` is not
  a `Result`; `waiting()` JoinError mapped; `load` missing-file safe).
- Validation: `cargo fmt -- --check`, `cargo clippy --all-targets --
  -D warnings`, full `cargo test` (see WP-10e entry for final counts).

### WP-10d visualizer

- `src/visualizer/mod.rs`: loopback-only HTTP/1.1 (no new deps):
  `GET /` index (counts + escaped titles/fragments, links `/api/library`),
  `GET /api/library` canonical export JSON, 404/405/400 shapes,
  per-request store open (never a second long-lived handle next to a
  daemon), `DEFAULT_VIS_PORT = 18721` (ltmrs-native, no upstream default on
  record). `-vis` backgrounds via detached `setsid` child + listen-poll +
  ownership probe (refuses port squatters); `-vis --fg` serves until Ctrl-C.
  Missing store is a usage error (`no store at ...`, never created).
- `src/main.rs`: `Visualize` arm wired (prints URL / farewell, exit codes
  preserved). `Cargo.toml`: tokio `net` + `signal` (pure Rust).
- `tests/vis_smoke.rs` (new): background detach live — parent exits 0 with
  `serving at ... (pid ...)`, index serves, reported pid killed, port goes
  quiet. Live-verified also: fg serves + Ctrl-C farewell exit 0; 404/405.
- Reviews: formal (ownership probe, pre_exec doc, accept-loop continue) +
  functional (brace repair after review patch; `ToSocketAddrs` via
  `Ipv4Addr`; `'static` spawn bounds via owned `String`; pipe-hang in smoke
  test → spawn-based, never `wait_with_output` on inherited stderr).

### WP-10e host recipes

- `src/skills/hosts.rs` (new): mechanism-based recipes
  (`managed-skill-dir`, `mcp-stdio`, `visualizer`) with exact skill path
  (pinned identical to the installer's `.agents/skills/ltmrs/SKILL.md`),
  exact activation commands, and `documented (supported recipe, not
  verified host behavior)` status — no invented host names/versions.
  `--install-skill` output now appends the documented recipe count, keeping
  "installed" and "host loaded it" separate (T-HOST-01).
- RED caught a test bug (status `"not verified"` contains `"verified"` →
  assertion now checks the disclaimer positively).
- Validation (all three slices): `cargo fmt -- --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test`:
  461 lib + stdio_smoke (1) + vis_smoke (1) passed, 0 failed.

### WP-10 remainings: skill-workflow pins, legacy shim, server-name proof

- Skill workflow vs compatibility names (`src/skills/installer.rs`):
  `skill_tool_tokens` extracts tool references from the native skill;
  `skill_workflow_tools_match_frozen_names` pins every referenced tool
  against `frozen_tools()` and the recall → act → persist core;
  `multilingual_guidance_is_separate_and_documented` pins the standalone
  multilingual section.
- Legacy shim (`src/skills/shim.rs`, `--install-shim` CLI command):
  opt-in `~/.local/bin/lemma` symlink to the running binary; foreign files
  AND foreign symlinks refuse (never replaced); PATH collisions reported
  (PATH order, shim excluded, non-executables ignored) but never block.
  `install_shim_command` + parser exclusivity + help coverage + live-binary
  verification (install → already-current; real collision with the legacy
  `~/.local/bin/lemma` on this machine warned correctly).
- Server-name independence (`src/frontend/mcp.rs`):
  `routing_needs_no_server_name` proves routing depends only on tool names
  across all 26 frozen tools (namespaces stay host-configured per hosts.rs).
- Reviews (formal + functional): own-shim spelling tolerance
  (`check_current` canonicalizes; `Path` equality normalizes `.` away, so
  the test uses a symlinked parent); AlreadyExists re-read for concurrent
  installers (matched on `ErrorKind`, never message text); empty-PATH-entry
  CWD guard; clippy `collapsible_if` collapsed to a let-chain.
- Trackers: `plans/02_implementation_guide.md` WP-10 tasks 1-8 checked
  (task 8 with recorded deviation: mechanism-based recipes, no per-host
  version claims).
- Validation: `cargo fmt -- --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test`:
  470 lib + stdio_smoke (1) + vis_smoke (1) passed, 0 failed.



## 2a07449 (2026-09-26) — WP-10a CLI parser with help/version and real library output


## cdc0bac (2026-09-26) — whole-learning-workflow trace through tool handlers


## a11c946 (2026-09-26) — dense memory preload at session start


## 0258341 (2026-09-26) — per-channel virtual sessions for session-less calls


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

## a46a111 (2026-09-24) — WP-09 guides, sessions and intelligence (7/10 tasks)

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

## 339fa58 (2026-09-23) — WP-08 memory handlers and differential wire harness (8/9 tasks)

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

## 89f7e58 (2026-09-20) — WP-07 retrieval, graph context and explanations

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

## dcc2e99 (2026-09-18) — WP-06 Candle embedding service and model qualification

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

## 7cf7aff (2026-09-17) — WP-05 versioned Lance search projection

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

## dcdb257 (2026-09-17) — WP-04 singleton daemon, IPC and session routing

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

## d5e8d40 (2026-09-16) — WP-03 canonical repository hardening

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

## edd09a7 (2026-09-16) — WP-00 baseline capture, dependency lock and gap closure (S0)

(single entry: history squash collapsed the baseline capture and the plan-
review gap closure into edd09a7; bodies preserved below.)

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
- Gap closure from plan review: static tool-definition snapshot
  (`static/tools_static.json`) kept separate from the live `tools/list`;
  DB schema version (8) and snapshot refs in `upstream-lock.json`; behavior
  classification in the inventory; `tools/list_changed` documented with real
  error captures; tokenizers aligned to 0.22.2 (593 -> 590 packages);
  native-code audit extended (F-05, model-license deferral).

## 9614885 (2026-09-16) — initial repository with implementation plans

- Added the reviewed implementation specification and test plan (`plans/`):
  design & concepts, implementation guide (WP-00…WP-13), quality/tests/benchmarks/conformance,
  plus `traceability.json` and `conformance_matrix.json`.
- Added `README.md` (overview, status, architecture, constraints) and `AGENTS.md`
  (agent working rules, plan authority, evidence discipline).
- Added dual license (`LICENSE-MIT`, `LICENSE-APACHE`), `rust-toolchain.toml` (1.96.0 baseline),
  Cargo skeleton, `.gitignore`, and IDE/MCP configs.
