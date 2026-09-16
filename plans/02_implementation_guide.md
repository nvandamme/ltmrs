# ltmrs - Part II: Implementation Guide

**Revision:** 2.0, 2026-09-15  
**Status:** ordered work plan, not implemented code  
**Authority:** [Part I](01_design_and_concepts.md) defines the contracts; [Part III](03_quality_tests_benchmarks_conformance.md) defines the evidence.

## 1. How to use this guide

Create issues from the work packages below. Every issue carries a requirement ID, command/slice boundary, prerequisite, acceptance test and saved evidence path. All checkboxes are intentionally unchecked. No code, benchmark or compatibility pass is implied by this package.

Use one Cargo package with a library and binary initially. Keep database crates and Arrow types behind storage/search modules. Expose domain types to the rest of ltmrs. Do not implement a general plugin/database framework merely to conduct the comparison.

The first work is **baseline capture and executable domain semantics**, not a polished CLI or a speculative final schema. A backend cannot be selected without running the critical command tests.

## 2. Delivery slices and dependency graph

| Slice | User-visible demonstration | Main work packages | Exit evidence |
|---|---|---|---|
| S0 - Reproducible baseline | Inspect a frozen upstream tool/behavior baseline and a buildable dependency set | WP-00, WP-01 | Pinned manifest, generated contract, reference interpreter tests |
| S1 - Atomic memory | Create, update, relate and merge a small memory set; kill/reopen without partial state | WP-02, WP-03 | Backend gate report and canonical consistency traces |
| S2 - Two real clients | Two separate stdio MCP frontends share one store but not implicit sessions | WP-04, early WP-08 | Host-independent wire traces, reconnect and scope tests |
| S3 - Lexical memory | Add/update/delete then recall lexically; show explicit index readiness | WP-05, early WP-07 | Projection race tests and FTS freshness probes |
| S4 - Real semantic memory | Use the pinned Candle model; run hybrid recall and visible no-model fallback | WP-06, WP-07 | Reference-vector and search fixtures |
| S5 - Graph-aware recall | Retrieve a correction and an unresolved conflict without misleading context | WP-07, WP-08 | Fixed-budget graph/contradiction regression cases |
| S6 - Whole learning workflow | Recall, act, persist, practice a guide, record attempts and end independent sessions | WP-09, WP-10 | Tool-family and supported-host conformance |
| S7 - Safe interchange | Import a coherent Lemma store, export, preview and restore with a safety backup | WP-11 | Loss accounting, round-trip and interrupted-restore evidence |
| S8 - Release qualification | Install and operate offline on a declared supported target | WP-12, WP-13 | Reproducible benchmark, dependency and release reports |

Dependencies:

```text
WP-00 baseline ------> WP-01 model/oracle ------> WP-02 backend gate
                                                |
                                                v
                                             WP-03 canonical
                                              /           \
                                    WP-04 daemon       WP-05 projection
                                        |                 |
                                   early WP-08         WP-06 model
                                        |                 |
                                        +----> WP-07 retrieval
                                                    |
                                                WP-08 memory
                                                    |
                                                WP-09 workflow
                                                  /       \
                                            WP-10 UX   WP-11 interchange
                                                  \       /
                                               WP-12 qualification
                                                       |
                                                  WP-13 release
```

WP-04 may develop against the reference repository while WP-02 runs. WP-06 can test model recipes independently. Do not merge assumptions about the selected storage into shared domain modules until the gate closes.

## 3. Repository layout

```text
Cargo.toml
Cargo.lock
rust-toolchain.toml
README.md
LICENSE-MIT
LICENSE-APACHE
NOTICE
src/
  lib.rs
  main.rs
  config.rs
  error.rs
  domain/                 # IDs, models, validated commands, invariants
  service/                # command orchestration, scope, receipts, sessions
  storage/                # selected canonical adapter and snapshots
  search/                 # Lance projection, candidate queries, generation metadata
  projection/             # desired-state jobs, retries, compare-and-clear
  embeddings/             # model manifest, Candle adapter, bounded worker
  retrieval/              # filters, parent collapse, RRF, graph, MMR, context
  daemon/                 # lock/lifecycle, local IPC, client registry, scheduling
  frontend/               # rmcp stdio and CLI, no duplicate domain handlers
  compatibility/lemma/    # frozen wire DTOs, responses and behavior adapters
  skills/                 # owned assets and installer, not arbitrary executable plugins
  interchange/            # import/export/backup/restore
  visualizer/             # local UI server and routes if compatibility requires them
migrations/
tests/
  model/
  storage/
  concurrency/
  recovery/
  projection/
  embeddings/
  retrieval/
  compat/lemma_0_21_0/
  security/
  hosts/
  fixtures/
benches/
  storage.rs
  graph.rs
  search.rs
  service.rs
tools/                    # development-only oracle/capture/report utilities
reports/                  # ignored generated results; release evidence is archived
```

During the spike, keep `experiments/lance_canonical` and `experiments/fjall_canonical` small and use the same command fixtures. Once AD-01 closes, remove the losing adapter from the production dependency tree. Do not delete its benchmark report or failing regression cases.

## 4. Work package format and definition of done

Each work package below defines owner role, dependencies, tasks, outputs and acceptance. One person can fill several roles; a second reviewer should sign off storage/recovery and compatibility claims.

A work item is ready only when input shape, allowed preconditions, canonical effects, failure states, scope, durability and expected evidence are specified. A work item is done only when its end-to-end test passes, relevant invariants/property tests pass, error paths are covered, and the implementation introduces no undocumented API change.

Rust snippets and commands in this document are design-level examples or planned repository commands. They are not presented as a compiled application delivered with this plan.

## 5. WP-00 - Freeze sources, dependencies and compatibility inputs

**Owner:** integration/conformance maintainer  
**Depends on:** none  
**Requirements:** RQ-03, RQ-15, RQ-21, RQ-28

### Tasks

- [ ] Check out Lemma commit `d30a816632d0bc5d92907cbc51c1dc1010111986`; verify package version 0.21.0 and license.
- [ ] Capture package lock, Node/runtime version, upstream config defaults, source schema versions and asset hashes.
- [ ] Build the pinned upstream in an isolated development environment; do not point it at the user's home or production store.
- [ ] Generate the static tool definitions from the built module; compare against the expected 29-name inventory.
- [ ] Capture real `initialize`, `tools/list`, `tools/call` and relevant notifications, including text plus structured results and errors.
- [ ] Inventory instructions, tool-description injection, virtual sessions, skill content/installer, CLI options, visualizer routes and backup formats.
- [ ] Record which observed behaviors are schema-documented, handler-defined, side effects, or apparent upstream defects. Do not copy a safety defect merely to make an exact-output test pass.
- [ ] Resolve a local published LanceDB/Fjall/Candle/rmcp dependency set; pin Cargo.lock and the actual toolchain. Use no `latest`, mutable Git branch or prerelease-only API in a release claim.
- [ ] Audit features, `links` crates, build scripts, C/C++/assembly dependencies and model licenses; distinguish build tools, optional import helper, CPU runtime and GPU runtime.
- [ ] Establish sandbox home, deterministic clock/IDs where possible, fixtures and documentation conventions.

**Outputs:** `upstream-lock.json`, static and live tool snapshots, default-config inventory, dependency/native-code report, proposed `deviations.json`.

**Acceptance:** T-BUILD-01, T-MCP-01 capture prerequisites and T-GATE-01. Generated snapshots include reproducible provenance. Network/tooling failures leave `not_captured`, never a hand-written "golden" pretending to be captured.

## 6. WP-01 - Domain types and executable reference model

**Owner:** domain maintainer  
**Depends on:** WP-00 inventory  
**Requirements:** RQ-04, RQ-05, RQ-12, RQ-16

### Tasks

- [x] Define validated ID newtypes, external aliases, revisions, store generation and channel/session identities.
- [x] Define all canonical record types, preserving optional quality, lifecycle distinctions, evidence, archives, guide dependencies, attempts and suggestions.
- [x] Define native DTOs separately from exact legacy wire DTOs; preserve missing/null distinctions where upstream observes them.
- [x] Define `DomainCommand`, `CommandContext`, `CommandReceipt`, `DomainError`, `Scope` and `SnapshotToken` without Arrow/database types.
- [x] Specify before/after state transitions for every critical command in Part I's atomic operation table.
- [x] Implement an in-memory sequential reference interpreter using deterministic IDs/clock. It is the oracle for concurrency histories, not a production backend candidate.
- [x] Implement graph endpoint, edge uniqueness, symmetry/direction, supersession-cycle and lifecycle predicates.
- [x] Define canonical normalized export and digest ordering for round-trip/property tests.
- [x] Map every legacy field into a known canonical field, legacy envelope, explicit derived field or rejected/loss-report category.

**Outputs:** validated domain model, sequential interpreter, generated schema/property fixtures, migration field map.

**Acceptance:** T-DATA-01, T-GRAPH-01 and interpreter fixtures for T-CONC-01/02. Serialization round-trips preserve unknown/null values and aliases. No database choice is embedded in domain logic.

## 7. WP-02 - Backend capability and correctness gate

**Owner:** storage maintainer plus independent reviewer  
**Depends on:** WP-00 and WP-01  
**Requirements:** RQ-01, RQ-02, RQ-03, RQ-18

### A. Lance-only probe

- [x] List exact public local Rust operations for conditional update, insert-if-absent, affected-row reporting, snapshot reads and multi-record publication.
- [x] Probe local normalized tables first; distinguish a local transaction implementation from remote namespace API availability.
- [x] Test `set_unenforced_primary_key`/index assumptions explicitly: metadata must not be mistaken for a uniqueness constraint.
- [x] Implement create, conditional edit, feedback, relation creation, merge and operation receipts through one proven atomic command path.
- [x] Run two concurrent absent-key creates and two stale-revision updates with deterministic barriers.
- [x] Run merge plus concurrent reads/updates/deletes and kill/reopen at each publication boundary.
- [x] Prove a coherent snapshot for graph traversal and export.
- [x] Test FTS/vector queries on empty stores, absent/null vectors, fresh append/update/delete, and after reopening.
- [x] If needed, conduct only one single-table alternate probe. Document how a command and its receipt become atomic through a supported API. Do not implement an ad hoc transaction framework.

### B. Fjall plus Lance probe

- [x] Implement the same commands with optimistic cross-keyspace transactions and explicit durable ACK behavior.
- [x] Read revision/uniqueness predicates inside the transaction; verify both the outer storage result and inner conflict result.
- [x] Demonstrate bounded retries with no external side effects inside the retry closure.
- [x] Commit pending projection work with canonical state; kill between canonical and Lance commits.
- [x] Use the same fixtures, durability promise, request trace and workload as A.

### Decision

- [x] Execute all hard gate tests before throughput scoring.
- [x] Compare the minimum necessary recovery states, mutation code, maintenance, direct-read/graph latency and resource use.
- [x] Record AD-01 with selected release/feature configuration, counterexamples and evidence links.
- [x] Remove the losing production path; retain the backend-independent tests.

**Outputs:** capability matrix, command implementation probes, raw histories, crash artifacts, AD-01.

**Acceptance:** T-STORE-01/02/03, T-CONC-01/02/03, T-REC-01/03, T-GATE-01. A failure of atomicity, durability or isolation is disqualifying, not a low performance score.

## 8. WP-03 - Harden the canonical repository

**Owner:** storage/domain maintainer  
**Depends on:** selected backend from WP-02  
**Requirements:** RQ-01, RQ-04, RQ-06, RQ-12

### Tasks

- [ ] Implement migrations with version checks, staging/safety rules and refusal of unknown/newer incompatible schemas.
- [x] Centralize command application, precondition validation and atomic receipt storage.
- [ ] Add scoped operation IDs and request digests; implement fixed-expiry daemon-issued retry namespaces, receipt retention, and stale replay rejection across reconnect/restart.
- [x] Separate storage conflict retries from stale edit conflicts and unknown commit outcomes.
- [x] Enforce memory/alias/edge uniqueness and referential/lifecycle invariants.
- [x] Make supersession checks safe against simultaneous cycle-forming insertions, including predicate dependencies or a scope-wide graph-mutation lock held through publication; merging components must not defeat lock coverage.
- [ ] Specify deletion effects on adjacency, evidence, guide links, receipt history and pending projections.
- [x] Provide snapshot-consistent multi-get, graph-neighbor and export traversal APIs.
- [ ] Persist compatibility-visible feedback/access effects correctly; separate diagnostic telemetry from domain state.
- [ ] Add fault injection around commit, receipt publication and migration steps.

**Outputs:** selected repository, migration runner, durable command gateway, crash-stable receipt lookup, domain inspector.

**Acceptance:** all storage/concurrency/graph cases pass against the reference interpreter; RQ-18 ACK timing includes its actual durability barrier. The inspector reports corruption/unresolved import references rather than silently repairing semantic knowledge.

> **Warning:** any temporary/test Fjall database must open at a unique OS-temp
> directory, never a real or CWD path — Fjall's `temporary(true)` deletes that
> directory on drop. See the "Critical pitfall" section in
> `plans/AD-01_canonical_backend.md`.

## 9. WP-04 - Singleton daemon, IPC and session routing

**Owner:** runtime/integration maintainer  
**Depends on:** WP-01; real integration after WP-03  
**Requirements:** RQ-05, RQ-20, RQ-22

### Tasks

- [ ] Implement private runtime directories, OS lock, secure socket creation and safe stale-socket recovery.
- [ ] Implement startup arbitration with multiple simultaneous frontends; validate daemon identity, store generation and IPC version.
- [ ] Implement one frontend-side rmcp boundary and one daemon-side domain dispatcher.
- [ ] Carry frontend/channel identity, scope, operation ID and deadline on every IPC request.
- [ ] Bind implicit legacy sessions per channel; add an explicit native session handle path without changing legacy schemas.
- [ ] Define reconnect/lease/idle-exit behavior and session abandonment rules; preserve history across restart.
- [ ] Add bounded request/response frames, streamed large results, per-client quotas and cancellation handling.
- [ ] Use dedicated blocking storage work and an embedding scheduler rather than blocking Tokio I/O workers.
- [ ] Coordinate shutdown/restore with pending commands and background jobs; keep a receipt available for a committed request whose connection disappears.
- [ ] Add health/doctor output showing readiness, active store, projection state and resource budgets without dumping memory contents.

**Outputs:** daemon lifecycle, IPC envelope, frontend registry, session binding and scheduler.

**Acceptance:** T-SESS-01/02, T-SEC-01/02, T-CONC-03/04. Thirty-two concurrent frontend start attempts produce one owner, not corrupt stores or mutually deleting sockets. One channel's `session_end` cannot end another's session.

## 10. WP-05 - Versioned Lance search projection

**Owner:** indexing/storage maintainer  
**Depends on:** WP-03, capability results from WP-02  
**Requirements:** RQ-07, RQ-08

### Tasks

- [ ] Define search rows using domain IDs, document revision, generation, model fingerprint, chunk identity, scope/type/date and rendered text.
- [ ] Implement lexical-ready rows even when no embedding exists; verify actual null-vector/filter support in the pinned backend.
- [ ] Create correct FTS/scalar indexes and query them with typed predicates.
- [ ] Implement durable desired-state jobs and compare-and-clear acknowledgements for the selected architecture.
- [ ] Serialize projection publication per entity/generation or use a verified atomic revision guard; prevent late old embeddings from overwriting newer rows.
- [ ] Treat events as retryable wakeups, not an event-sourced canonical history or a UUID-ordered commit log.
- [ ] Implement tombstones/delete propagation and a rebuild path that cannot resurrect deleted generations.
- [ ] Make model/index generation changes blue-green: build new, catch up revisions, publish atomically, retain rollback until readers drain.
- [ ] Define lexical and semantic readiness separately; expose lag and oldest pending age.
- [ ] Schedule index optimization and retention with explicit disk/memory budgets and snapshot protection.
- [ ] Test query readers reopening/refreshing after commits and generation changes; do not rely on a cached handle being automatically current.

**Outputs:** projection schema, job state machine, readiness API, rebuild/optimization commands.

**Acceptance:** T-PROJ-01/02/03 and T-SEARCH-01. Replay after a crash converges to current canonical state; no highest-UUID checkpoint is used. A stalled embedder cannot prevent direct reads or lexical indexing.

## 11. WP-06 - Candle embedding service and model qualification

**Owner:** inference/retrieval maintainer  
**Depends on:** WP-00 model/artifact baseline; integration with WP-05  
**Requirements:** RQ-09, RQ-10, RQ-21, RQ-22

### Tasks

- [ ] Pin the initial E5-small repository revision and file digests; record redistribution rights and reference environment.
- [ ] Match its BertModel configuration and XLM-RoBERTa tokenizer artifacts; do not substitute generic WordPiece assumptions.
- [ ] Implement document/query prefixing, padding/attention masks, masked pooling, normalization and finite-value checks.
- [ ] Run the reference model in development-only tooling and save approved reference vectors/tokens and tolerance rationale.
- [ ] Implement a bounded synchronous Candle worker with async service calls, request cancellation and batch accounting.
- [ ] Implement safe tokenizer-length-aware derived chunking while retaining the original memory ID and exact offsets.
- [ ] Separate native model adapters from Lance embedding registration; supply vectors explicitly.
- [ ] Add explicit artifact fetch, cache verification and offline-only load modes.
- [ ] Test CPU F32 first. Add CUDA only with separate build, numerical, resource and host tests.
- [ ] Reject unsupported model recipes with an actionable message rather than trying any arbitrary safetensors model.

**Outputs:** model manifest, worker/service, validated adapter, reference fixtures, supported-model entry.

**Acceptance:** T-EMB-01/02/03, T-SEC-04. Passing tests must include padded mixed-length batches, prefix asymmetry, tail-of-memory retrieval and normalization. The model list contains only qualified adapters.

## 12. WP-07 - Retrieval, graph context and explanations

**Owner:** retrieval maintainer  
**Depends on:** WP-03, WP-05, WP-06  
**Requirements:** RQ-11 to RQ-14, RQ-24

### Tasks

- [ ] Implement direct-ID and empty-query routing separately from ranked search.
- [ ] Resolve effective scope once; apply it to both candidate legs, canonical hydration and every graph step.
- [ ] Implement identifier-preserving lexical behavior and multilingual test cases.
- [ ] Execute lexical and dense candidate queries separately initially for explanations and reference scoring.
- [ ] Collapse derived chunks to parent memories before fusion; retain matched spans.
- [ ] Implement deterministic one-based RRF, the legacy reference scorer and the normalized native scorer as separate tested functions.
- [ ] Add bounded graph expansion with edge-specific policies, limits, provenance and no unlimited hub summation.
- [ ] Resolve supersession chains and build protected conflict/correction bundles before MMR.
- [ ] Implement semantic/lexical diversification with explicit score normalization, missing-vector behavior and stable tie-breaking.
- [ ] Add no-answer rules, candidate backfill, readiness/partial-result reporting and finite-score validation.
- [ ] Budget actual serialized context; label approximate token accounting when the target tokenizer is unknown.
- [ ] Record explanations for this call: candidate ranks, filters, revision/generation, graph paths, score components, diversification decisions and excluded/truncated context.

**Outputs:** recall engine, context assembler, explanation schema mapping, retrieval profile version.

**Acceptance:** T-SCOPE-01/02, T-SEARCH-01/02, T-RANK-01/02/03/04 and frozen retrieval fixtures. Protected stale/conflict/scope cases cannot be sacrificed to improve average nDCG.

## 13. WP-08 - Complete memory MCP contract

**Owner:** compatibility maintainer  
**Depends on:** WP-00, WP-04, WP-07; minimal tools begin at S2  
**Requirements:** RQ-15, RQ-17

### Tool scope

`memory_read`, `memory_add`, `memory_update`, `memory_feedback`, `memory_forget`, `memory_merge`, `memory_relate`, `memory_stats`, `memory_audit`, `memory_library`, and `semantic_search`.

### Tasks

- [ ] Capture and test `memory_add.confirm` verbatim-storage behavior. Preserve it in the approved legacy policy, or reject explicitly and record the stricter policy deviation; never silently transform confirmed content.

- [ ] Reuse frozen input/output schemas and implement explicit legacy DTO conversion.
- [ ] Match absent/null/default/enum/case-sensitive fields and unknown-field handling from the actual contract.
- [ ] Match structured/text results, `response_format`, explain output, pagination and error classes.
- [ ] Reproduce observable confidence/access/context tagging, deduplication, consolidation, invalidation and archival behavior.
- [ ] Add native enhancements only through separately negotiated configuration or additional native tools, not new required legacy arguments.
- [ ] Implement instructions and dynamic tool-description context scoped to the frontend's project/channel.
- [ ] Snapshot static schemas separately from generated dynamic context; test appropriate notification behavior.
- [ ] Write differential upstream traces for each tool and approved retrieval deviations.

**Outputs:** eleven functioning tool handlers, context integration, compatibility ledger updates.

**Acceptance:** T-MCP-01/02/03/04 and T-CLI-01 relevant parts. No placeholder or successful empty response counts as a port. A "read-only" annotation is not used to ignore an upstream read side effect.

## 14. WP-09 - Guides, sessions and intelligence

**Owner:** workflow/domain maintainer  
**Depends on:** WP-03, WP-04, WP-08  
**Requirements:** RQ-05, RQ-12, RQ-15, RQ-16, RQ-17

### Tool scope

Seven guide tools; five session/suggestion tools; `conflict_scan`, `proactive_analysis` and `project_analytics`.

### Tasks

- [ ] Port guide create/read/update/forget/merge, contexts/learnings, dependencies, deprecation and source/validation links.
- [ ] Match guide practice statistics, success/failure semantics and interactions with session completion.
- [ ] Port traced and virtual session lifecycles per frontend channel.
- [ ] Persist attempts with explicit outcomes and related-memory mappings; do not infer hidden reasoning.
- [ ] Ensure session end is retry-safe and cannot double-count guide outcomes.
- [ ] Persist suggestion acceptance/dismissal and reproduce surfaced suggestion state.
- [ ] Port actual heuristic conflict/proactive behavior; distinguish suggestions from accepted canonical edges.
- [ ] Use dense search to propose relevant candidates, not as proof of contradiction, support or truth.
- [ ] Implement analytics over canonical snapshots without unbounded read transactions.
- [ ] Extend the upstream conformance traces across the full recall -> act -> persist workflow.

**Outputs:** all workflow/intelligence handlers and history views.

**Acceptance:** T-SESS-01/02, T-MCP-02/04, T-DATA-01, T-GRAPH-02. Interleaved attempts from multiple frontends remain correctly attributed. Shared-channel subagent behavior is explicitly documented, not guessed.

## 15. WP-10 - CLI, managed skills, hosts and visualizer

**Owner:** integration/UX maintainer  
**Depends on:** WP-04, WP-08, WP-09  
**Requirements:** RQ-15, RQ-20, RQ-25

### Tasks

- [ ] Implement native commands and the exact claimed legacy aliases, exit codes, stdout/stderr separation and argument errors.
- [ ] Implement managed skill installation, update, ownership/hash markers, atomic writes and explicit replacement of foreign assets.
- [ ] Test the pinned upstream skill workflow against the compatibility tool names; keep native multilingual guidance separate and documented.
- [ ] Provide an opt-in legacy executable shim; detect PATH collisions without silently replacing an existing installation.
- [ ] Verify client-configured server names/tool namespaces rather than relying on `serverInfo.name` alone.
- [ ] Implement the visualizer invocation/foreground/port behavior and the claimed functional routes. Graph rendering itself need not be pixel-identical to upstream.
- [ ] Use loopback binding, local access controls and safe output encoding for any UI; a localhost UI is still a separate input surface.
- [ ] Record which host versions discover which skill path and how the workflow is activated.

**Outputs:** CLI help/reference, installers, host recipes and functional visualizer.

**Acceptance:** T-CLI-01, T-SKILL-01, T-HOST-01, T-SEC-03. "Installed" and "host loaded it" are separate evidence fields. Node/Python remain unnecessary for running ltmrs.

## 16. WP-11 - Import, legacy interchange, backup and restore

**Owner:** persistence/recovery maintainer plus reviewer  
**Depends on:** WP-03 and the complete canonical field map; full workflow integration after WP-09  
**Requirements:** RQ-16, RQ-19, RQ-26

### Tasks

- [ ] Freeze supported Lemma schema/backup versions and format readers from actual fixtures.
- [ ] Implement coherent source snapshotting, including SQLite WAL and supported legacy sidecars; never mutate the original.
- [ ] Implement loss-accounted field/ID/relation migration, preserving nullable fields, archives, evidence and dependency/history links.
- [ ] Reject or quarantine invalid references explicitly; retain raw source metadata for manual repair.
- [ ] Implement native logical backup with one snapshot, manifest counts/digests and atomic final-file publication.
- [ ] Implement actual legacy backup import/export or mark the corresponding conformance target unsupported; never relabel a native archive.
- [ ] Implement preview, lease checks, state/digest/channel binding and legacy-compatible token expiry.
- [ ] Implement maintenance barrier, verified safety backup, staged restore, active-generation switch and rollback/restart protocol.
- [ ] Invalidate pending embedding/index tasks from the previous store generation.
- [ ] Add bounded archive parsing/extraction and malicious archive tests.

**Outputs:** import/export codecs, loss reports, backup coordinator, restore state machine.

**Acceptance:** T-IMPORT-01/02, T-BACKUP-01/02/03 and T-REC-02/03. Reimport/export is idempotent where promised; unsupported fields are counted. Recovery yields a valid old or new store, not an undetected mixture.

## 17. WP-12 - Qualification, quality and performance

**Owner:** QA/performance maintainer  
**Depends on:** early harness at WP-02; final runs after S7  
**Requirements:** RQ-23, RQ-24

### Tasks

- [ ] Implement the deterministic generator and state-history recorder shared by all backend experiments.
- [ ] Run storage-only tests with fixed precomputed vectors before mixing inference cost into the result.
- [ ] Run closed-loop concurrency and open-loop offered-load tests with latency measured from scheduled arrival.
- [ ] Record durability, queue time, conflicts, retries, projection lag, maintenance state, disk growth and memory alongside throughput.
- [ ] Run fault and soak tests at steady state, not only on a freshly created unfragmented database.
- [ ] Build development and held-out retrieval labels, split by topic/project to prevent leakage.
- [ ] Run ablations for lexical, dense, hybrid, priority, graph and MMR at the same context budget.
- [ ] Measure no-answer false positives, obsolete advice, conflict coverage and cross-scope leakage in addition to conventional IR scores.
- [ ] Calibrate candidate pools, thresholds, model choice and inference scheduling only on the development split.
- [ ] Publish raw results and analysis scripts with every architecture/ranking decision.

**Outputs:** performance report, quality report, raw histories/histograms and gate results.

**Acceptance:** T-BENCH-01/02/03, T-QUALITY-01/02. Unrun tests are `not_run`; tests without enough tail samples are marked insufficient evidence. No headline comparison omits durability or model cost.

## 18. WP-13 - Release, documentation and evidence closure

**Owner:** release maintainer  
**Depends on:** all required slices and passing hard gates  
**Requirements:** RQ-21, RQ-27, RQ-28

### Tasks

- [ ] Run formatting, lint, unit, integration, property, recovery, conformance and target-specific build suites.
- [ ] Check the CPU binary and resolved dependencies against the audited native-code policy.
- [ ] Validate an offline installation from a fresh user directory with pre-provisioned model artifacts and no network access.
- [ ] Test migration from the prior ltmrs schema and supported Lemma sources; verify rollback instructions.
- [ ] Publish supported tool/workflow, host, model, OS and durability matrices.
- [ ] Review every enhancement/deviation and remove unsupported absolute claims.
- [ ] Archive source lock, Cargo.lock, binary digest, SBOM/license inventory, model manifest, fixture checksums, raw results and signed review decisions.
- [ ] Publish v0.1-alpha until all claimed full-surface release gates pass.

**Outputs:** release bundle, compatibility statement, operational guide and evidence index.

**Acceptance:** T-RELEASE-01 and all mandatory entries in Part III. The release checklist cannot waive atomicity, acknowledged knowledge loss, scope leakage or destructive-restore safety.

## 19. Priority TODO order

The first implementation sequence is intentionally short:

1. Capture the exact Lemma baseline and resolve the CPU dependency lock.
2. Implement the reference command model and the merge/receipt/conditional-update fixtures.
3. Run Lance-only's atomicity/uniqueness/local-API probes, then the same Fjall-plus-Lance probes.
4. Select a backend only after those pass; harden the canonical repository.
5. Demonstrate two real stdio frontends, correct session routing and recovery.
6. Add lexical projection, then qualified Candle inference, then GraphRAG.
7. Complete the tool/CLI/skill/workflow inventory and safe interchange.
8. Qualify and publish exactly the supported release surface.

Do not start by implementing every table and all 29 handlers against an unproven storage transaction assumption.

## 20. Planned repository automation

The implementation repository should provide a small `xtask` or equivalent development runner. These command names are a **target interface to implement**, not commands already delivered here:

```text
cargo xtask capture-lemma --source <pinned-checkout>
cargo xtask capabilities --candidate lance
cargo xtask capabilities --candidate fjall-lance
cargo xtask conformance --profile lemma-0.21.0
cargo xtask recovery --suite durable
cargo xtask benchmark --manifest benchmarks.toml
cargo xtask quality --split held-out
cargo xtask evidence --release <version>
```

Keep benchmark/test runners separate from production dependencies. A Python/Node upstream oracle in development does not violate the runtime constraint.

## 21. Handoff and change policy

The next developer should receive this pack, the pinned upstream checkout and a clean implementation repository. They should not have to reconstruct decisions from the conversation.

Changing a requirement requires updating Part I, its work package, the matching test and the conformance/deviation ledger in one review. Adding a backend/model/host is a capability and evidence task, not a README-only change. Performance tuning cannot silently weaken durability, scope filters or whole-command atomicity.
