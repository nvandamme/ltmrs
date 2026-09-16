# ltmrs - Part III: Quality Assurance, Tests, Benchmarks and Conformance

**Revision:** 2.0, 2026-09-15  
**Status:** test specification; product tests and benchmarks not executed  
**Companions:** [Design and concepts](01_design_and_concepts.md), [Implementation guide](02_implementation_guide.md)

## 1. Quality model

Quality is not a single score. This plan separates six questions:

1. Is canonical knowledge correct and durable under concurrency/failure?
2. Is the complete claimed Lemma contract actually preserved?
3. Does retrieval find applicable knowledge without leaking or misleading?
4. Is inference the selected model's correct computation?
5. Does the service meet declared latency/resource limits under a realistic offered load?
6. Can a user install, operate, migrate and recover it safely offline?

A failure in the first two cannot be offset by higher throughput. Retrieval quality and latency do not excuse scope leakage or returning obsolete advice as current. Safety-critical failures are release blockers.

### 1.1 Evidence states

Every matrix cell is one of `not_run`, `inspected`, `passed`, `failed`, `blocked`, or `waived_with_reason`. Source inspection is not execution. A waiver cannot cover acknowledged knowledge loss, partial canonical commands, unsafe restore, scope leakage, or a falsely claimed compatible tool.

This package's [evidence_status.json](evidence_status.json) reports only the work actually performed while writing the specification. The conformance matrix is a planned inventory. No latency result, crash pass or Candle output has been invented.

### 1.2 Test levels

| Level | Main oracle | Typical purpose |
|---|---|---|
| L0 - Pure/domain | Sequential reference model and arithmetic expectations | Revisions, lifecycle, graph invariants, RRF/MMR |
| L1 - Storage adapter | Same command fixtures on real local databases | Atomicity, uniqueness, durability, snapshots |
| L2 - Service | Real daemon plus multiple frontend processes | Identity, retry, cancellation, readiness, fair scheduling |
| L3 - Protocol/host | Pinned Lemma transcripts plus supported MCP hosts | Wire/workflow/skill/CLI compatibility |
| L4 - Recovery/security | Fault-controlled child/VM/storage harness | Process crashes, I/O faults, restore and isolation |
| L5 - Performance/quality | Fixed traces, labels, model manifests | Tail latency, saturation, retrieval effectiveness |

Keep the sequential reference model after selecting the backend. It is cheap and useful for regression tests; it is not a second production storage implementation.

## 2. Hard gates and decision procedure

| Gate | Required proof | Failure consequence |
|---|---|---|
| G0 - Reproducibility | Frozen sources, local release features, build/native-dependency audit | Do not claim the capability or publish a release |
| G1 - Atomic command | Whole mutation and receipt appear together to concurrent readers and after restart | Candidate rejected |
| G2 - Preconditions | Conditional update, absent-key uniqueness, edge invariants and safe replay | Candidate rejected |
| G3 - Durability | Acknowledged canonical writes survive the supported failure model | Candidate rejected for the declared durability contract; narrower target support requires an explicit reviewed requirement change, not a passing score |
| G4 - Isolation | No cross-channel session attribution or unintended cross-scope data | Release blocked |
| G5 - Recovery | Coherent snapshot, loss-accounted import and validated restore publication | Release blocked |
| G6 - Model/retrieval | Correct model recipe; current and applicable context; protected cases pass | Do not advertise semantic/GraphRAG support |
| G7 - Conformance | Every claimed tool/workflow/host row passes or has a clearly excluded scope | No full-surface compatibility claim |
| G8 - Operational qualification | Offline install, resource limits, maintenance and benchmark evidence | Release blocked for the affected target/profile |

Backend selection is a two-step decision. First eliminate candidates that fail G0-G3 and the relevant portions of G4-G5. Then compare the passing candidates on the workload and integration complexity. Raw line count is descriptive only; never use it as a substitute for reviewing custom recovery machinery.

A stable `PASS` gate requires an archived run, not a README statement. The decision report must include the operation trace that distinguished the candidates.

## 3. Functional and invariant test catalog

IDs below are referenced by `traceability.json`. Each implementation test should emit its ID into the report. A table row names a suite; individual test functions may add deterministic scenario suffixes.

### 3.1 Storage, revisions and concurrency

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-STORE-01 | Create A/B, merge into C with edges, aliases, lifecycle changes and receipt; read concurrently at every injected boundary. | Every canonical view is wholly before or wholly after the command; never C without required edges/receipt or only one source transition. |
| T-STORE-02 | Generate multi-object histories including guide/session updates, graph changes, delete/merge and read snapshots. | Observed histories admit a valid serial execution respecting real-time order of completed operations. |
| T-STORE-03 | Compile and execute each adopted API through the exact local Rust backend/feature set, including conditional mutations and snapshot export. | No remote-only, private or merely specified capability is used as proven local support. |
| T-CONC-01 | Barrier-start multiple updates at the same expected revision; then concurrent increments, disjoint-field edits and stale client edits. | Exactly the permitted writer succeeds; no lost updates; stale intent is not silently reapplied after retry. |
| T-CONC-02 | Run 32 independent sessions/keys, then concurrent create-if-absent for the same alias/edge key. | Independent writes are all retained; contested uniqueness has one winner or a documented idempotent equivalent. |
| T-CONC-03 | Lose a response after commit; reconnect and replay the same scoped operation; reuse its ID with different input; repeat after expiry and restore. | One effect within the replay contract; mismatched input rejected; expired/old-generation identities do not become new writes. |
| T-CONC-04 | Overload one client, delay inference, and run maintenance while another client issues short reads and writes. | Bounded queues/memory; explicit overload; no indefinite starvation; committed work retains receipts under cancellation. |
| T-GRAPH-01 | Insert/delete directional and symmetric edges; race endpoint deletion and edge creation; generate concurrent cycles and duplicates. | Endpoint, direction, uniqueness and cycle policies hold; forward/reverse views agree. |
| T-GRAPH-02 | Retrieve old and new claims, two conflicting replacements, archived/deleted nodes and out-of-scope graph neighbors. | Only eligible current advice is actionable; unresolved conflicts and filtered-out successors are handled explicitly. |
| T-DATA-01 | Round-trip every canonical/legacy field, null/absent value, alias type, evidence, archive, guide dependency and history reference. | No implicit unknown-to-zero conversion, lost fields or accidental enum extension. |

For linearizability testing, record invocation and response times, operation IDs, inputs, outputs, snapshots and durable receipts. A final correct record count does not prove absence of lost updates or invalid intermediate views.

Test shared hot memories separately from disjoint sessions. Do not assume write/write conflicts are handled correctly when neither operation records the relevant read/uniqueness dependency.

### 3.2 Sessions and frontends

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-SESS-01 | Interleave legacy start/attempt/end calls from at least 32 separately identified frontend channels, reusing the same MCP numeric request IDs. | Attempts/outcomes belong to the intended channel's session; request IDs do not collide globally. |
| T-SESS-02 | Reconnect, expire leases, restart daemon, open sibling subagents on the same channel, and use native explicit bindings. | Rebinding follows policy; no daemon-global current session; shared-channel ambiguity is documented and never guessed from prose. |

A same-process async task test does not replace the multiple-frontend-process test. The latter exercises the actual startup lock, IPC identity, reconnect behavior and shared model ownership.

### 3.3 Projection, freshness and search basics

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-PROJ-01 | Commit events in a different order from their UUID timestamps; kill between canonical commit, projection commit and pending acknowledgement; race compare-and-clear. | No late event is skipped; replay is idempotent; a newer desired revision stays pending. |
| T-PROJ-02 | Delay an old embedding, update/delete the memory, rebuild with a new model, then restore an older store generation. | Old jobs cannot overwrite or resurrect current state; generation/fingerprint guards reject stale publication. |
| T-PROJ-03 | Add/update/delete and immediately read by ID, lexically and semantically; stall the embedder; reopen cached Lance readers. | Direct read-your-writes holds; lexical/semantic readiness is accurate; obsolete hits are rejected or reported with an explicit limitation. |
| T-SEARCH-01 | Empty database/query, FTS not yet built, null/missing vectors, fresh unindexed tails and optimization concurrent with queries. | Clear valid outcomes; no crash or false completeness; exact/lexical routes remain available as specified. |
| T-SEARCH-02 | Queries for flags, environment variables, mixed-case identifiers, paths, punctuation, accents and long error strings. | Exact technical terms are not lost by generic stemming/tokenization; results have current canonical IDs and spans. |
| T-SCOPE-01 | Seed out-of-scope memories and linked graph traps; exercise all recall legs, direct IDs, explanations, guides and injection. | No unauthorized/unrequested scope expansion or metadata leak; filters apply consistently. |
| T-SCOPE-02 | Two native projects with the same basename, omitted versus explicit legacy scope, global inheritance, stale confidence and narrow date/type filters. | Correct identity resolution and eligibility; filtered candidates are backfilled or limitations are declared. |

Run projection tests with both **same-dimensional different models** and different-dimensional models. Dimension equality is not embedding-space compatibility.

### 3.4 Model and ranking tests

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-EMB-01 | Compare tokenizer IDs, special tokens, attention masks and prefixing to the pinned reference on FR/EN/code input. | The Candle adapter implements the selected recipe rather than a guessed architecture default. |
| T-EMB-02 | Compare single/batched embeddings on empty, short, padded mixed-length and near-limit inputs on CPU; repeat the approved GPU profile separately. | Correct shape, finite values, masked pooling, normalized outputs and reference equivalence within frozen tolerances. |
| T-EMB-03 | Put the only relevant fact beyond the first model window; vary Unicode/token density and paragraph boundaries. | Deterministic chunks preserve recall and parent identity; any remaining truncation is explicit. |
| T-RANK-01 | Check RRF against hand-computed ranks, missing legs, ties, duplicate chunks and empty rankings. | One-based reference arithmetic is exact within tolerance; normalized native values remain bounded. |
| T-RANK-02 | Compare a strongly relevant near-duplicate with irrelevant diverse candidates using old raw-scale and normalized scores. | The scale regression is detected; native MMR does not mechanically prefer noise because raw RRF is tiny. |
| T-RANK-03 | Near-identical contradictory claims, superseded advice and context budgets too small for a complete pair. | Protected conflict/correction bundles survive diversity; overflow yields a clear warning, not one misleading side. |
| T-RANK-04 | Zero lexical overlap, empty query, unrelated semantic neighbors, NaNs/zero vectors and highly popular irrelevant memories. | Valid no-answer behavior; invalid values excluded; rank/priority is not presented as truth. |

For CPU F32, an initial qualification target is maximum absolute reference-vector error <= 1e-4 and cosine agreement >= 0.99999 for nondegenerate vectors, with unit-norm error <= 1e-5. These are proposed thresholds to validate and freeze during adapter qualification, not current measurements. Store actual error distributions. GPU/mixed precision uses its own documented envelope and rank-equivalence tests.

Do not use an exact hash of floating-point vectors as the sole cross-platform oracle. Keep exact checks for integer token IDs and manifests; use numerical tolerances for floating point. Any widened tolerance requires an explained change review, not just a failing-test workaround.

The MMR regression can use this analytical fixture: with raw relevance 0.08 and similarity 0.8 at lambda 0.70, the near-duplicate utility is -0.184, while an irrelevant item with raw relevance 0.01 and zero similarity scores 0.007. On a calibrated unit scale, relevance 1.0 versus 0.1 yields 0.46 versus 0.07 for the same similarities. This illustrates the scale bug; it is not a claim that all near-duplicates should always be kept.

## 4. Recovery, durability and fault injection

### 4.1 Failure classes must not be conflated

| Failure class | Harness | What it can establish |
|---|---|---|
| Graceful restart | Clean shutdown/reopen | Ordinary persistence and resource cleanup |
| Process crash | Child-process SIGKILL at controlled boundaries | Behavior when userspace stops; OS page cache may survive |
| Storage API faults | Inject EIO/ENOSPC, short writes, failed flushes and corrupt reads | Error handling, uncertain outcomes and refusal/recovery paths |
| Modeled power loss | A block/VM/storage harness that discards or reorders only writes not protected by modeled durability barriers | Durability under that explicitly documented model |
| Hardware power-loss qualification | Controlled dedicated test machine/storage environment | Hardware/filesystem-specific evidence, not universal proof |

**SIGKILL alone is not a power-loss test.** Killing a VM process may also leave host caches intact; describe exactly what the harness discards and which flushes it honors.

### 4.2 Recovery suites

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-REC-01 | Run commands with an external ACK ledger; kill before mutation, during commit, after durable commit and before response. Reopen and compare to the reference history. | All acknowledged durable commands survive; unacknowledged outcomes resolve to a valid committed/uncommitted state and receipts prevent duplication. |
| T-REC-02 | Combine disconnect/cancel/retry with projection, migration and generation changes. | No duplicated canonical effect, lost valid receipt or replay into the wrong generation. |
| T-REC-03 | Inject I/O faults, corruption and modeled loss of unflushed writes at selected engine/application boundaries. | Never report durable success before the barrier; corruption is detected or safely recovered; no silent partial canonical state. |

Keep the ACK ledger in the harness outside the process/device being failed. A successful `commit()` return is not automatically accepted as a durability barrier: document the selected engine's actual local path and setting. For Fjall, test the explicitly chosen durability mode; default OS-buffer flushing is not equivalent to `SyncAll`. [S14, S15]

Run each deterministic failpoint repeatedly and include seeded randomized histories. Log the failing seed, exact command schedule and last successful barrier. A count-only check is insufficient: verify relationships, session attribution, revisions, aliases, lifecycle states and receipts.

### 4.3 Backup and import suites

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-BACKUP-01 | Export while normal writes occur; compare the logical result with its declared snapshot; restore into a new empty store. | One coherent canonical cut; all supported records/links/counts/digests preserved. |
| T-BACKUP-02 | Preview then change the file, source state, client, token, leases or store generation; reuse a token; expire it. | Wrong/stale/unconfirmed restores are rejected before replacement; no authorization inferred from archive content. |
| T-BACKUP-03 | Fail before/after safety backup, staged validation, active-pointer publication and reopening. Include disk-full and archive attacks. | Startup uses a complete verified old or new generation; original live data is not irrecoverably overwritten. |
| T-IMPORT-01 | Import every supported Lemma fixture, including null quality, evidence, archives, guide dependencies, legacy reverse edges, suggestions and sidecar history. | Every field/reference is preserved, mapped or explicitly loss-reported; no silent skip. |
| T-IMPORT-02 | Import a coherent live-WAL snapshot and an offline copy; include malformed source/unsupported schema; instrument writes to the original. | Snapshot consistency and no importer mutation/migration of the original; unsupported inputs fail safely. |

An actively changing source can change because its own upstream writer is running. The test distinguishes those writes from importer writes; it does not falsely require an active source's bytes to remain constant. For offline fixtures, compare original content hashes directly.

Archives must reject absolute paths, `..`, duplicate conflicting entries, symlink/hardlink escapes, oversized expansion, truncated compression and manifest/count mismatches. A digest is corruption detection, not authentication of a malicious archive.

## 5. MCP, CLI, skills and workflow conformance

### 5.1 Baseline and comparison rules

Freeze the upstream repository/version/commit and generated assets before porting. Capture actual requests and responses from the isolated upstream process. Keep source-generated tool definitions separate from the live `tools/list` response, whose descriptions can contain dynamic memory. Include the explicit `memory_add.confirm` redaction bypass in the captured privacy-policy matrix; stricter behavior is a documented deviation, not silent compatibility.

Normalize only unstable data deliberately: generated IDs through a bijection, frozen/injected timestamps where possible, temp paths and documented nondeterministic ordering. Do not normalize away field presence, numeric/string type, side effects, error class, skipped records or unexpected tool calls.

Schemas are compared semantically or through a narrowly documented canonicalizer. Tool descriptions/instructions have static and dynamic tests. Host-dependent presentation is not a valid reason to omit `structuredContent` validation when an output schema is declared. MCP distinguishes protocol errors from tool execution errors. [S18]

### 5.2 Suite IDs

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-MCP-01 | Compare initialize capabilities, tool names, schemas, defaults, enums, annotations and supported notifications against the frozen source/wire baseline. | No missing/renamed tool, extra mandatory legacy input or schema drift. |
| T-MCP-02 | Run minimal, maximal and multi-step valid calls for all 29 tools on equivalent fixtures; compare responses and canonical state. | The complete claimed functional surface is implemented, not just the schema. |
| T-MCP-03 | Invalid/missing/unknown arguments, malformed protocol, wrong enum/date/ID, unsupported methods and oversized requests. | Correct protocol-versus-tool error boundary and safe resource behavior. |
| T-MCP-04 | Trace read/access/confidence/tagging, duplicate-add, feedback, session end and guide practice side effects. | Observable legacy effects are preserved or listed as explicit deviations; no hidden eventual-consistency change. |
| T-CLI-01 | Exercise no-argument stdio, help/version, `-lib`/`--library`, `-vis`/`--visualize`, foreground/port options and install-skill behavior. | Correct alias behavior, exit status and clean protocol stdout; unsupported visualizer behavior is not advertised. |
| T-SKILL-01 | Install/update in a temporary home; modify the file; install a foreign upstream file; simulate write denial/concurrent installers. | Ownership-aware, idempotent and atomic updates; foreign/user content protected. |
| T-HOST-01 | Use each claimed host/version to discover tools/skill, invoke recall/write, reconnect and preserve session scope. | Record actual supported behavior; installation alone does not count as discovery or workflow activation. |

### 5.3 Per-tool conformance matrix

Each row receives all of: schema/default validation; text/JSON/error comparison; state transitions; scope; concurrency/retry; restart; upstream oracle comparison; and enhancement review. The machine-readable matrix starts every check at `not_run`.

| Tool | Behavior that must not be omitted | Owning package |
|---|---|---|
| memory_read | Summary/full/IDs modes, filters, pagination, observable access effects | WP-08 |
| memory_add | Validation, duplicate policy, `confirm` privacy override, evidence, session links, conflict/proactive side effects | WP-08 |
| memory_update | Partial fields, revision/lifecycle behavior, provenance, index invalidation | WP-08 |
| memory_feedback | Exactly one feedback effect, confidence/counters and logging | WP-08 |
| memory_forget | Exact forget/invalidate/archive/consolidate distinctions supported upstream | WP-08 |
| memory_merge | Source preservation/removal semantics, links and atomic visibility | WP-08 |
| memory_relate | Direction/inverse/symmetry, duplicate policy and scope | WP-08 |
| memory_stats | Snapshot consistency, optional/nullable metrics and project/global behavior | WP-08 |
| memory_audit | Actual integrity/maintenance signals, not an unconditional healthy result | WP-08 |
| memory_library | Complete snapshot/maintenance content and bounded transport streaming | WP-08 |
| semantic_search | Dense/native enhancement is explicit; flags, pagination and explanations preserved | WP-08 |
| guide_get | Discovery/detail, dependencies, contexts and deprecation | WP-09 |
| guide_practice | Usage/outcome counters, learnings and current-session interactions | WP-09 |
| guide_create | Validation, uniqueness and relationships | WP-09 |
| guide_distill | Supported non-LLM/agent-supplied distillation behavior and source memories | WP-09 |
| guide_update | Partial changes, dependencies, lifecycle and preserved metadata | WP-09 |
| guide_forget | Exact deletion/deprecation/reference behavior | WP-09 |
| guide_merge | Atomic outcome and reference/history mapping | WP-09 |
| session_start | Channel-local traced/virtual state, guide/preload response | WP-09 |
| session_attempt | Ordered attempts, outcome/critique fields and related-memory references | WP-09 |
| session_end | Terminal transition and guide outcomes counted once | WP-09 |
| session_stats | Traced/legacy history and project scoping | WP-09 |
| suggestion_respond | Numeric legacy IDs, accept/dismiss and future surfacing behavior | WP-09 |
| conflict_scan | Heuristic candidate generation, explicit conflicts and documented side effects | WP-09 |
| proactive_analysis | Actual signals/suggestions and state transitions | WP-09 |
| project_analytics | Correct scoped aggregates with snapshot semantics | WP-09 |
| backup_create | Real supported portable format, complete inventory and verification | WP-11 |
| backup_preview | Readiness/leases, digest/state binding, expiry and clear replacement preview | WP-11 |
| backup_restore | Explicit approval, single-use token, verified safety backup and usable connection after restore | WP-11 |

The number 29 is an expected baseline inventory, not a substitute for the source/wire capture. The captured count and schemas are authoritative after WP-00.

### 5.4 Conformance classes and truthful claims

Report separate classes:

- **Wire:** tool names, schemas, envelopes and negotiation.
- **Workflow:** state transitions, effects, scopes, sessions and guides.
- **Integration:** CLI aliases, skill assets/ownership and tested host activation.
- **Interchange:** supported DB/backup import/export versions and loss accounting.
- **Retrieval:** reference semantics versus intentionally enhanced dense/GraphRAG results.

A retrieval enhancement can be approved without producing identical TF-IDF rankings, but it must not be represented as exact result equivalence. A deferred visible confidence update, missing backup reader or unimplemented guide tool is not a harmless ranking enhancement.

The deviation ledger stores: affected upstream commit/tool; previous behavior; ltmrs behavior; reason; user-visible impact; test; release wording; and approval. Security fixes may intentionally differ but must remain named and tested.

### 5.5 Host matrix

Begin with the user's principal OpenCode workflow and separately test Claude Code/Codex only where installed test versions are available. Record host version, OS, server configuration key, protocol negotiation, tool naming, skill directory, discovery evidence, recall/write workflow, reconnect and subagent routing. A missing environment is `blocked`, not `passed`.

Do not claim all hosts use the same directory or inject MCP instructions in the same way. Never count a model's successful one-off response as a deterministic guarantee of future skill activation.

## 6. Security, privacy and resource tests

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-SEC-01 | Wrong UID/permissions, stale socket, simultaneous daemon starts, symlinked runtime paths and mismatched store generation. | Unauthorized/wrong-store connection rejected; one safe owner; no unsafe unlink or takeover. |
| T-SEC-02 | Fuzz IPC frames/JSON, query strings, evidence paths and size/depth limits; test cancellation at every stage. | Bounded parsing and memory; no injection/path escape or arbitrary filesystem reads. |
| T-SEC-03 | Store malicious memory/skill content that asks to run commands, leak secrets or change restore approval; exercise visualizer rendering. | Content remains data; no privileged instruction promotion, auto-execution or unsafe HTML/script injection. |
| T-SEC-04 | Known-secret fixtures, false positives, logs/explanations, model digest tampering, offline mode and forbidden remote code/model formats. | Supported secret patterns are handled before downstream use; artifacts are verified; no unapproved egress or code execution. |
| T-BUILD-01 | Build the locked CPU release and optional profiles; inspect dependency features/native linkage and run offline install. | The declared runtime/native-code/license policy is supported by actual artifacts. |

No finite secret fixture set establishes perfect secret detection. Report supported detection classes and residual risks. Per-user IPC protects other UIDs; it does not stop a malicious process already running as the same user from reading that user's database files.

## 7. Benchmark strategy

### 7.1 Three distinct measurements

**Storage-only:** real domain commands and graph reads, precomputed immutable vectors, no model load/inference. Compare atomicity/durability-equivalent candidates.

**Retrieval-only:** fixed corpus, index state, query vectors and context budget. Measure lexical, exact dense, optional ANN, fusion, graph hydration and formatting separately.

**End-to-end:** real frontend -> IPC -> queue -> storage/search -> Candle -> context -> frontend. This is the user-perceived latency. Include batching wait, retry and durability time.

Reporting only the storage kernel while excluding fsync, queuing or inference cannot establish the end-to-end target.

### 7.2 Reference environments

R0 is a declared ordinary development machine: at least 8 physical CPU cores, 32 GiB RAM, local NVMe and a named Linux filesystem. Record the exact CPU, available cores, RAM limits, disk model, filesystem/mount options, kernel, power settings, toolchain and crate locks. Do not claim measurements for the user's workstation unless actually run there.

R1 adds the actual supported NVIDIA/CUDA configuration. It is supplementary; CPU correctness and no-GPU fallback remain required.

Cold/warm state must be explicit: process restarted, model loaded or absent, OS file cache cold/warm, Lance index built/unbuilt, projection backlog, compaction active/inactive and snapshot age.

### 7.3 Dataset and workload matrix

| Dimension | Planned values |
|---|---|
| Memories | 1k, 10k, 100k |
| Relationships | 5 per memory average; plus skewed high-degree hotspots |
| Content | Short facts, realistic technical text, long/chunked memories; matched length distributions |
| Vectors | Fixed normalized 384-d data for backend comparison; real qualified model for E2E |
| Independent sessions | 1, 2, 4, 8, 16, 32; 64/128 as stress tiers |
| Access distribution | Uniform, hot-memory skew and project-isolated workloads |
| Requested mixes | 90/10, 70/30 and 50/50 read/write requests |
| Maintenance | Clean store, accumulated small writes, active optimization, stalled projection |
| Failure | Disconnect/retry, canceled work, worker restart and separate fault suites |

Read percentages classify **requests**, not physical writes. Lemma reads may update visible metadata; measure those writes instead of pretending the workload is read-only.

Use the same serialized operation trace, IDs, expected conflicts, vector bytes and initial state on each candidate. A batch of 32 commands is not comparable to 32 per-command fsyncs unless latency and ACK guarantees are matched.

### 7.4 Arrival models and measurement

Run both closed-loop clients (one outstanding request per session) and open-loop arrivals at declared rates, for example 10/50/100/250 requests per second until saturation. The rates are a measurement grid, not a promised capacity.

Measure open-loop latency from the scheduled arrival time to avoid hiding overload behind a stalled load generator. Report offered load, accepted load, rejected/busy requests and completed throughput. Include request queue time and errors, not only successful completions.

At minimum record p50/p95/p99, histograms, CPU, peak/steady RSS, logical bytes changed, physical bytes written, disk growth, maintenance pauses, conflicts/retries, fsync/barrier costs, oldest pending job age and search coverage. Tail percentiles require enough samples; a p99 from 100 requests is not strong evidence.

Use fixed seeds, a versioned generator and at least five independent measured runs after declared warmup. Archive raw samples; report confidence intervals and observed range. Use steady-state runs that include compaction/retention behavior and a longer release soak. Record the run duration rather than implying the short CI test proves long-term stability.

### 7.5 Proposed service targets

These are initial acceptance objectives for R0, not results. If a target proves unrealistic, revise it explicitly before release rather than excluding the expensive part of the operation.

| Operation/profile | Initial objective | Measurement boundary |
|---|---|---|
| Warm frontend startup | p95 <= 150 ms | Invocation to ready MCP frontend, existing daemon |
| Daemon readiness | <= 2 s without migration/model download | Process start to direct/lexical service availability |
| Canonical point read | p95 <= 10 ms | Service call including normal queueing |
| Durable canonical mutation | p95 <= 50 ms at 32-session/64-request-per-second reference mix | Request admission to durable ACK, including fsync and retries |
| Graph expansion | p95 <= 20 ms depth 1; <= 50 ms bounded depth 2 | Seed IDs to hydrated eligible graph context |
| Search service | p95 <= 150 ms at 10k; <= 300 ms at 100k, excluding model inference | Candidate query, hydration, graph/ranking/context |
| End-to-end warm recall | p95 <= 750 ms with qualified CPU model at declared offered load | Frontend request to final response, including inference/batch wait |
| Canonical loss | Zero acknowledged knowledge loss in supported fault suite | External ACK ledger versus recovered state |
| Scope/correctness | Zero protected-scope leaks or partial canonical commands | All required adversarial/concurrency cases |

Separately publish cold model load and embedding throughput; never hide them within the backend comparison. Performance at 64/128 concurrent sessions is a stress report unless independently promoted to a supported tier.

### 7.6 Benchmark suite IDs

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-BENCH-01 | Matched backend traces with identical durable ACK and batch policy. | Apples-to-apples comparison, full latency boundary and raw evidence. |
| T-BENCH-02 | Open-/closed-loop sessions, skew, inference delays and overload. | Useful throughput and tail latency measured without omitted queue time; fairness/resource bounds hold. |
| T-BENCH-03 | Repeated small updates, retention, deletes, index maintenance and soak. | Storage/latency do not silently degrade without reported limits; maintenance is budgeted. |

The supplied [benchmarks.toml](benchmarks.toml) is a versioned workload specification for a future runner, not a benchmark program or result file.

## 8. Retrieval quality and ANN conformance

### 8.1 Corpus construction

Start with at least 300 reviewed query cases and expand before broad performance/quality claims. Include exact identifiers, paraphrases, cross-language FR/EN, code/configuration, long-tail facts, corrections, contradictions, scope traps, time filters and genuinely unanswerable queries. Keep source topics/projects disjoint between development and held-out evaluation; do not tune on the held-out set.

Label relevance, applicability, obsolete/current status, mandatory conflict companions and allowed scope. Preserve the labeler rationale separately from model-generated answers. Real private memories require consent and must not be committed to a public fixture repository.

Measure Recall@k, MRR and nDCG at both fixed k and fixed serialized context budget. Also measure obsolete-advice rate, conflict coverage, no-answer false-positive rate, redundant-context rate and latency/token cost.

### 8.2 Ablations

Evaluate lexical only; dense exact only; lexical+dense RRF; plus priority; plus graph; plus MMR; and the pinned upstream retrieval reference where feasible. Use the same candidate/output budget and known model state.

Do not require GraphRAG to beat every baseline on every query. Require zero failures on deterministic safety/applicability cases, and predeclare an aggregate non-regression envelope plus the intended graph-specific benefit. A reasonable initial target is no more than 0.03 absolute MRR loss relative to the best applicable baseline on held-out data, with paired uncertainty reported. This is a proposed gate to approve before testing, not a claim of achieved quality.

For the held-out no-answer set, predeclare a false-positive ceiling (initial objective 5%) and report sample size/confidence interval. Scope leakage and presenting known obsolete advice as current remain zero-tolerance fixture failures regardless of aggregate scores.

### 8.3 ANN only after exact search

Exact dense search is the initial correctness baseline. If the measured 100k workload exceeds the search budget, evaluate an available local uncompressed ANN configuration on the pinned release. Do not assume an index name available in another SDK/backend is available in local Rust.

Use exact search as ground truth for ANN recall, distinct from human semantic relevance. An initial ANN quality target is Recall@10 >= 0.98 on the fixed query set, including narrow filters. Report memory, build/maintenance cost and actual latency benefit. An ANN mode that drops freshly inserted rows or defeats filters fails regardless of speed.

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-QUALITY-01 | Frozen labeled development/held-out corpus with full ranking ablations and fixed budgets. | Protected cases pass; non-regression/benefit claims are supported by labels and uncertainty. |
| T-QUALITY-02 | ANN versus exact on the same vectors, filter distribution and index freshness state. | Declared recall and freshness envelope holds; optimization has a measured purpose. |

## 9. CI, nightly and release execution

| Schedule | Required work |
|---|---|
| Every change | Formatting/lint/build, L0/domain tests, targeted storage tests, schema/compatibility fixtures, score/scope regressions |
| Pull request changing persistence | Real database atomicity, receipt/revision, projection races and selected deterministic process-crash cases |
| Pull request changing inference | Tokenization/reference-vector, mixed-batch and truncation tests on the affected profile |
| Nightly | Multi-process sessions, randomized state histories, broader crash/I/O suites, maintenance/soak and representative benchmarks |
| Release candidate | Full claimed host/OS/model matrix, offline packaging, supported power-loss model, complete import/restore, all tool conformance and frozen quality evaluation |

Cache model artifacts in CI through a verified manifest. A network outage should mark a missing artifact prerequisite, not silently skip model tests and produce a green semantic-support badge.

### 9.1 Evidence bundle

Each qualification run records:

```text
run.json                 # revision, environment, test/profile configuration
upstream-lock.json
Cargo.lock
model-manifest.json
fixture-digests.json
test-results.json
histories/*.jsonl
latency/*.jsonl
resource-samples/*.jsonl
quality-results.json
conformance-results.json
fault-recovery/*.json
known-deviations.json
review-signoff.md
```

Raw data must remain available to reproduce the report. Never place plaintext private memories, tokens or unrestricted logs in the public evidence bundle.

## 10. Design Q&A for implementers and reviewers

### Can LanceDB-only still win?

Yes. It is the first simplification candidate. It must demonstrate the actual local atomic-command, uniqueness, snapshot and durability contract. Its general ACID label, a REST namespace endpoint or a single-table schema is not sufficient evidence.

### Does a singleton daemon remove transaction requirements?

No. It can serialize operations and prevent some interleavings, but a process crash between separate durable writes can still produce partial state. The transaction/publication protocol must remain correct after restart.

### Does Fjall's optimistic mode guarantee faster multi-agent behavior?

No. It permits concurrent transaction preparation and can reject conflicts, but commit/durability work still serializes in relevant places. Short transactions, hot-key contention and barrier cost must be measured fairly. [S14, S15]

### Why retain a pending projection mechanism when Lance can scan unindexed rows?

Because Lance can scan only rows that have reached Lance. Canonical writes not yet projected, stale model work and generation changes require their own correctness protocol. [S13]

### Are UUIDv7 IDs a global commit cursor?

No. Generating an ID precedes commit, and independently prepared operations can commit out of generation order. Use desired revisions and explicit acknowledgements, not a highest-seen-ID assumption.

### Can we preserve full Lemma schemas and add expected revisions/session IDs?

Preserve the old schemas in the compatibility adapter. Native extensions or frontend context carry extra information. Adding mandatory arguments to the legacy tools is a breaking change. A host that multiplexes indistinguishable subagents onto one old channel cannot magically acquire separate sessions.

### Can access counts be batched away?

Only if doing so preserves the observable contract or is an explicitly documented native behavior. A read that changes confidence/tags/stats in upstream cannot be treated as a pure read merely because its annotation says read-only.

### Is Candle model support just a `model_type` match?

No. E5-small combines a BertModel configuration with an XLM-RoBERTa tokenizer and a particular pooling/prefix recipe. Qualification covers tokenizer artifacts, masks, pooling, normalization and limits, not only weight loading. [S16, S17]

### Are graph corrections simply higher scores?

No. A trusted applicable supersession changes which advice is current. Conflicts may require presenting both sides. Neither is reliably modeled by a small additive score or unprotected MMR diversity.

### Does a native backup satisfy the Lemma backup tools?

Not automatically. The tool contract and the file format are separate compatibility targets. Supported `.lemma-backup` reading/writing must be implemented and tested; native-only data needs a loss report on legacy export.

### Does passing SIGKILL establish power-loss durability?

No. It establishes behavior for a dead userspace process while the OS may retain dirty pages. Qualification must name the storage fault/power-loss model and the actual durability barriers.

### Can a skill installer guarantee recall -> act -> persist?

No. It can safely install correct instructions. Host discovery and model activation need separate tests and remain host/model dependent. Do not force knowledge text into privileged context to compensate.

### Is the package already a tested implementation?

No. It is a reviewed design, ordered implementation guide and test specification. Artifact validation checks that this pack is coherent; it does not certify a database, model or MCP server.

## 11. Final release checklist

- [ ] AD-01 backend selection has passing hard gates and a reviewed evidence report.
- [ ] Dependency/model/profile baselines are immutable and reproducible.
- [ ] Every claimed tool has schema, behavior, scope, retry and restart evidence.
- [ ] No partial canonical commands, lost acknowledged knowledge or scope leaks remain.
- [ ] No-model and stalled-index behavior is honest and useful.
- [ ] Concurrency tests use real independent frontends and the documented session model.
- [ ] Model reference and protected retrieval cases pass.
- [ ] Backup/import/restore tests cover coherent snapshots and interrupted publication.
- [ ] Offline runtime and native-dependency policy pass on each supported target.
- [ ] Performance and quality claims include exact measurement boundaries and raw data.
- [ ] Skill/host compatibility is evidenced per tested host version.
- [ ] Release notes clearly distinguish complete API/workflow support from enhanced retrieval and any exclusions.

| Test ID | Procedure | Required assertion |
|---|---|---|
| T-GATE-01 | Validate the requirement/evidence matrix and backend/release decision report. | No unrun/failed mandatory gate is mislabeled as passed or hidden by an aggregate score. |
| T-RELEASE-01 | Execute the release checklist and audit the advertised scope against the evidence bundle. | Every claim has evidence; all hard blockers are closed; remaining exclusions are explicit. |
