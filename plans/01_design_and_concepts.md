# ltmrs - Part I: Design and Concepts

**Revision:** 2.0, reviewed 2026-09-15  
**Status:** implementation specification; no backend selected by experiment yet  
**Supersedes:** the preceding 57-section *Revised Implementation and Validation Plan*  
**Companions:** [Implementation guide](02_implementation_guide.md) and [Quality, tests, benchmarks and conformance](03_quality_tests_benchmarks_conformance.md)

## 1. Review verdict

The previous plan had a useful product direction but was not yet a reliable implementation contract. It mixed verified library capabilities with hypotheses, left crucial multi-agent behavior implicit, and allowed performance scoring to obscure correctness. It also called behavior "fully compatible" before a versioned oracle existed.

This revision keeps the agreed direction: Rust-first, permissive/open dependencies, local operation, a shared daemon, the entire Lemma tool/workflow surface, Candle-owned embeddings, and LanceDB retrieval. **LanceDB-only remains the simplification candidate; Fjall plus LanceDB remains the transactional fallback. Neither has passed the required ltmrs tests.**

The central correction is this: choose the smallest design that satisfies the domain contract, not the database with the longest feature list. A recoverable partial canonical merge is not equivalent to an atomic merge. A fast buffered write is not equivalent to an acknowledged durable write. A schema-compatible tool with different hidden effects is not automatically behavior-compatible.

### 1.1 Evidence categories

- **Verified source fact:** supported by an inspected upstream source or public API, identified in the [source register](SOURCE_REGISTER.md).
- **Design decision:** a normative ltmrs requirement, not a claim about implemented code.
- **Hypothesis:** must be resolved by the assigned capability probe or benchmark.
- **Executed evidence:** a saved command, environment, result and artifact. None of the database, model or conformance tests in this pack have been executed.

The JSON inventories shipped with this pack are plans, not fabricated wire captures or passing test reports.

### 1.2 Review findings

| ID | Severity | Original sections | Review finding | Required correction |
|---|---|---|---|---|
| RV-01 | BLOCKER | 2-4, 14, 37, 42, 54, 57 | A per-table commit was treated too much like an arbitrary multi-entity transaction. | Require a supported local atomic command path, including its receipt, and snapshot-consistent readers. A crash-repairable partial canonical operation does not pass. (RQ-01) |
| RV-02 | BLOCKER | 4, 38-42, 54 | Weighted scores could compensate for a correctness failure. | Use pass/fail correctness gates first. Compare performance and integration complexity only among passing candidates. (RQ-02) |
| RV-03 | HIGH | 2, 17, 28, 37 | Main-branch features, stable Rust APIs, and namespace/server APIs were conflated. | Freeze release, feature set, source revision and local capability probes. A REST endpoint or format specification is not proof of embedded Rust support. (RQ-03) |
| RV-04 | BLOCKER | 12, 29-30 | A revision field alone does not prevent lost updates; unique IDs are not enforced merely by index metadata. | Atomically compare revisions and absent keys, validate affected rows, and commit all results or none. Test duplicate ID/edge insertion and absent-key races. (RQ-04) |
| RV-05 | BLOCKER | 5-8, 29, 32 | Session ownership was not defined across frontends, reconnects and subagents. | Bind implicit legacy sessions to authenticated frontend channels. Never use a daemon-global current session. Shared-channel subagents require explicit native binding or are documented as one legacy session. (RQ-05) |
| RV-06 | BLOCKER | 29-33, 41 | Retries, cancellation and lost responses had no durable operation contract. | Store a scoped idempotency receipt with the canonical mutation; distinguish storage conflicts, stale user edits and unknown commit outcomes. (RQ-06) |
| RV-07 | BLOCKER | 19, 29, 39, 47, 56 | UUIDv7 IDs were implicitly being used as a safe ordered projection checkpoint. | Use durable per-entity pending work and compare-and-clear acknowledgements. IDs are identities, not commit order. (RQ-07) |
| RV-08 | HIGH | 3, 19-20, 28, 33, 47 | Projection lag, stale results and missing embeddings were underspecified. | Separate canonical durability, lexical freshness and semantic freshness; validate generation and document revision; never return an obsolete vector result as current. (RQ-08) |
| RV-09 | HIGH | 16-19, 46 | The model loader contract was too generic. | Validate a pinned model recipe: architecture plus tokenizer, attention masks, pooling, prefixes, normalization and artifacts. E5-small declares BertModel but XLMRobertaTokenizer. (RQ-09) |
| RV-10 | HIGH | 17-19, 46-47 | One memory was assumed always to fit one embedding input. | Preserve canonical memory identity but permit deterministic derived chunks; detect truncation and test queries whose answer lies beyond the first model window. (RQ-10) |
| RV-11 | HIGH | 20-27, 44-45 | Raw RRF/priority values and unit-scale cosine were combined in MMR without calibration. | Keep an upstream-reference scorer for tests, normalize native relevance explicitly, bound graph and priority contributions, and calibrate on a held-out corpus. (RQ-11) |
| RV-12 | HIGH | 13-15, 23-25 | Graph integrity and the distinction between authoritative corrections and similarity bonuses were missing. | Specify endpoint constraints, edge direction/symmetry, uniqueness, cycle policy and protected correction/conflict bundles. (RQ-12) |
| RV-13 | BLOCKER | 20-28, 43-45 | Filters could be applied only after retrieval or lost during graph expansion. | Apply one effective scope to every retrieval leg and graph step; enforce current eligibility on hydration and backfill filtered candidates without silently claiming completeness. (RQ-13) |
| RV-14 | HIGH | 20-28, 44-45 | There was no no-answer policy or exact-identifier retrieval contract. | Test empty/no-match queries, identifier-preserving tokenization and semantic false positives; never interpret nearest-neighbor rank as proof of relevance. (RQ-14) |
| RV-15 | BLOCKER | 7-11, 48-49 | Full compatibility was stated before actual wire and behavioral baselines existed. | Freeze Lemma 0.21.0 by commit; capture tools, handlers, instructions, side effects, CLI and skill artifacts; maintain an explicit enhancement/deviation ledger. (RQ-15) |
| RV-16 | HIGH | 12-15, 36, 50 | The proposed domain and importer omitted nullable fields, evidence, archives and parts of guide/session history. | Use a complete field inventory and loss-accounted import; preserve unknown legacy fields and external IDs; do not coerce unknown quality to zero. (RQ-16) |
| RV-17 | HIGH | 31, 33, 48 | Batching visible access counters/confidence can change Lemma behavior. | Persist contract-visible read side effects before success; batch only semantically equivalent groups or explicitly lossy diagnostic telemetry. (RQ-17) |
| RV-18 | BLOCKER | 33, 35, 41-42, 51 | Durable-write latency excluded the cost that defines durability, and SIGKILL was overinterpreted. | Include the durability barrier in ACK latency. Separate process-crash, storage-fault and modeled power-loss evidence. (RQ-18) |
| RV-19 | BLOCKER | 35-36, 41, 50 | Backup/restore lacked a complete consistent-cut and publication protocol. | Snapshot all canonical state; bind confirmation to digest, store generation and state; stage, validate and atomically publish a restored generation. (RQ-19) |
| RV-20 | HIGH | 5-6, 32, 34 | Local IPC, path safety and query construction lacked a trust model. | Use same-user authenticated IPC, private directories, an OS singleton lock, bounded framing, safe path opens and typed predicates. (RQ-20) |
| RV-21 | HIGH | 34, 46, 56 | Secret detection and Rust purity were described as absolutes. | State bounded detection guarantees; audit feature-resolved native dependencies and model licenses; separate release runtime from test/import tooling. (RQ-21) |
| RV-22 | HIGH | 5, 16, 32, 51 | Concurrent sessions had no queue, resource or fairness policy. | Bound in-flight work and inference batches; reserve interactive capacity; define overload, cancellation and maintenance behavior. (RQ-22) |
| RV-23 | HIGH | 38-45, 51, 54 | Benchmarks lacked fair durability, arrival models, maintenance and confidence intervals. | Use identical traces and durability; separate embeddings; measure queue time, tails, retries, lag and storage growth at steady state. (RQ-23) |
| RV-24 | HIGH | 44-47, 52 | GraphRAG was required to beat every baseline without defined labels or statistical criteria. | Require no regression on protected cases and evaluate quality at a fixed context budget on frozen held-out labels. Improvements are empirical, not guaranteed. (RQ-24) |
| RV-25 | HIGH | 9-11, 48-49, 52 | Skill installation was confused with client discovery and reliable activation. | Test installer ownership separately from each host client; preserve foreign files and do not claim installation forces an agent workflow. (RQ-25) |
| RV-26 | HIGH | 36, 41, 50 | Importing a live SQLite database read-only was treated as sufficient isolation. | Read a supported coherent snapshot, accounting for WAL and sidecar state, and prove the original source is unchanged. (RQ-26) |
| RV-27 | MEDIUM | 37, 52-55 | Horizontal phases deferred conformance, recovery and user-visible validation. | Build vertical slices with tests in each slice; freeze the contract first and prove atomicity before implementing the large tool surface. (RQ-27) |
| RV-28 | HIGH | 1-57 | The plan did not distinguish design decisions, verified facts, hypotheses and executed evidence. | Every requirement has an owner work package, tests and an evidence status. This pack is a reviewed specification, not a tested implementation. (RQ-28) |


### 1.3 Coverage of the previous plan

| Original sections | Review scope | Principal findings |
|---|---|---|
| 1-4, 54, 57 | Product constraints, storage hypotheses and selection | RV-01 to RV-04, RV-21, RV-28 |
| 5-6, 29-33 | Process model, concurrent sessions, retries and durability | RV-05 to RV-08, RV-17, RV-18, RV-20, RV-22 |
| 7-11, 48-49 | MCP, CLI, skill and client compatibility | RV-15, RV-25 |
| 12-15 | Canonical models and graph storage | RV-04, RV-12, RV-16 |
| 16-19, 46-47 | Model correctness, long inputs and model migration | RV-08 to RV-10 |
| 20-28, 44-45 | Candidate retrieval, scoring, graph expansion and context | RV-11 to RV-14, RV-24 |
| 34-36, 50 | Privacy, backups and migration | RV-16, RV-19 to RV-21, RV-26 |
| 37-43, 51 | Capability tests, crash tests and benchmarks | RV-01, RV-02, RV-18, RV-23 |
| 52-53, 55-56 | Delivery sequence, repository boundaries and invariants | RV-27, RV-28 |

All 57 sections are covered; the review does not simply replace the backend paragraph.

## 2. Product boundary

`ltmrs` means **Long Term Memory RS**, with the "long timers" pun. It is a local memory service for coding and other LLM agents, not a general database product, code indexer or autonomous reasoning engine.

### 2.1 Required behavior

A user can attach several MCP clients, persist facts and lessons, record guides and attempts, relate knowledge, recall by identifiers or meaning, receive applicable corrections/conflicts, and move the complete knowledge store to another machine. The service continues providing direct reads and lexical recall without an embedding model.

The agent/client remains responsible for reasoning and deciding what to save. ltmrs stores explicit findings and concise attempt summaries; it does not require hidden chain-of-thought capture. Distillation uses the upstream non-LLM behavior or agent-supplied content. There is no hidden cloud model call.

### 2.2 Constraints

The production CPU profile must not require Node, Python, ONNX Runtime, or a C++ database engine. Rust-native core components are preferred. This is **not** a promise that every transitive system/compression dependency is Rust or that optional CUDA uses no vendor libraries. Publish the feature-resolved audit rather than using a blanket "100% Rust" label. [S09, S14, S20]

Use OSI-approved software dependencies, retain upstream notices, and audit model-weight/tokenizer licenses separately. New project code may use MIT OR Apache-2.0; translated MIT Lemma code must retain its notices. A license inventory is a release input, not a substitute for review of redistribution obligations.

First supported target: local Linux x86_64 CPU. Cross-platform design is required, but macOS/Windows support is claimed only after their IPC, storage, restore and packaging suites pass. CUDA is an optional independently tested feature, not a prerequisite for basic memory.

No remote database, distributed consensus, automatic entity ontology, general Cypher engine, cloud sync, automatic semantic graph links, or hidden LLM summarization is required for v0.1.

### 2.3 Release naming

Use `v0.1-alpha` for the useful vertical slice. The full **v0.1 compatibility release** is gated on the complete supported Lemma inventory, data interchange, safety and host tests. Do not call an eleven-tool alpha "full Lemma compatibility."

## 3. Source baselines and change control

The inspected Lemma package is `lemma-mcp` **0.21.0** at commit:

```text
d30a816632d0bc5d92907cbc51c1dc1010111986
```

The package version and branch SHA were checked. The exact runtime wire transcript still has to be captured from this commit. [S01, S02]

The reviewed tool inventory contains the 29 names from the draft; WP-00 must compare that inventory against the generated source and live `tools/list`. A count mismatch blocks the baseline freeze rather than being silently normalized.

At review time, the documentation showed LanceDB 0.38.0, Fjall 3.1.10, rmcp 3.2.0 and Candle documentation for 0.11.0. These are **observed candidate versions, not a resolved or compiled Cargo set**. WP-00 must select mutually compatible published releases, pin Cargo.lock, record feature flags and transitive native dependencies, and derive the actual minimum Rust version. Earlier hard-coded dependency combinations are withdrawn. [S09, S14, S19, S20]

Each adopted capability needs five fields: source/version, public Rust entry point, local/remote backend applicability, test ID, and evidence status. This matters because Lance has both a table transaction specification and a REST namespace API for atomic multi-table version creation; neither alone demonstrates that the selected embedded Rust configuration supplies an arbitrary multi-record transactional API. [S11, S12]

## 4. Architecture decision, without another speculative backend switch

### 4.1 Common architecture

```text
MCP host A -> ltmrs stdio frontend --+
MCP host B -> ltmrs stdio frontend --+-> private local IPC -> ltmrs daemon
CLI / visualizer client -----------+                         |
                                              domain commands and snapshots
                                                            |
                                              selected canonical repository
                                                            |
                                         versioned lexical/vector projection
                                                            |
                                            LanceDB + Candle worker
```

One daemon owns one physical store. Multiple logical stores may exist, but each has its own identity, lock, runtime endpoint and generation. Sharing a UID is not multi-user authentication.

### 4.2 Candidate A: one storage engine, LanceDB

Candidate A stores canonical state and retrieval state using Lance. Start by testing a normalized domain: memories, relationships, guides, sessions, attempts, feedback, aliases and operation receipts.

Before building the complete application, prove that the selected **local published Rust API** can atomically publish the write set of a logical operation, validate its preconditions, and let readers take a consistent domain snapshot. If a supported local catalog transaction exists, exercise it. Do not assume that a remote namespace method is available locally.

A single-table representation is permitted as one narrowly scoped alternative probe. It must still demonstrate conditional multi-row updates, unique identities, receipts and snapshot behavior using supported operations. Merely moving records into one table does not turn several `update()` calls into one transaction.

**Disallowed shortcuts:** a home-built distributed transaction coordinator; an event-sourcing framework introduced only to rescue the candidate; weakening visibility to "repair it after restart"; copying the entire knowledge base on every mutation without meeting the workload limits; or adopting unstable/private APIs without a separately approved maintenance commitment.

An in-process command mutex can prevent some concurrent interleavings. It cannot make two separately committed files/tables crash-atomic. Conversely, serialized short writes are not inherently disqualifying: they must meet the same service-level workload as any other design.

Even Candidate A retains a logical separation between canonical knowledge and derived embeddings/search fields. "One engine" does not imply "no asynchronous derived-state lifecycle."

### 4.3 Candidate B: Fjall canonical state, Lance retrieval

Fjall owns canonical objects, aliases, adjacency indexes, receipts, and pending projection work. A logical mutation uses one cross-keyspace optimistic transaction. Lance owns only rebuildable search documents and vectors. [S14, S15]

Canonical transactions are short. Validation against previously read state is performed inside the transaction. Embedding inference, Lance searches, filesystem evidence reads, and unrelated network work are outside it. Retry only the transaction, not the user's reasoning or external side effects.

Fjall's optimistic commit still has a serialized validation/publication path, and its durability mode is explicit. Concurrent sessions alone do not establish a throughput advantage. The fallback must pass the same end-to-end durability and contention tests. [S15]

### 4.4 Non-negotiable selection gates

Both candidates must pass: atomic command visibility; conditional update/uniqueness; durable ACK recovery; consistent snapshots; deterministic receipt replay; scope isolation; coherent export/restore; offline local Rust integration; and license/dependency audit.

Only then compare latency, resource use, maintenance burden and integration complexity. Correctness has no weight that can be averaged away. Do not use the previous arbitrary "within 20%" rule.

Select A if it passes and is materially simpler across the actual critical operations. Select B if A cannot meet the contract without a transaction/recovery subsystem of its own. If neither passes, keep the decision open with a failing test and blocker; do not publish a spurious winner.

The selected backend is fixed in the production build. Preserve the domain contract and shared tests; remove the losing production adapter after recording the decision.

## 5. Canonical consistency contract

### 5.1 Command semantics

A successful knowledge mutation means its complete canonical effect and operation receipt have reached the configured durable commit point. A subsequent direct read beginning after the ACK must observe that mutation or a later valid state.

Concurrent canonical commands must admit a serial explanation respecting completed-before-started operations. Compound reads use one consistent canonical view. The externally visible operation boundary matters more than the engine's marketing use of "ACID" or "MVCC."

A command is one of: successful with receipt; rejected without effects; or outcome unknown to the caller. Unknown outcomes are resolved using the receipt and the same operation key. A transport timeout is not proof that a write failed.

### 5.2 Idempotency and conflicts

Use a durable key `(store_generation, frontend_retry_epoch, operation_id)` plus a canonical request digest. The daemon issues each authenticated frontend a retry namespace with a fixed expiry, separate from its renewable channel/session binding. The frontend creates a new operation ID for a new tool invocation and retains it for IPC retries. An MCP JSON-RPC request ID is not a global deduplication key. Two identical intentional calls must not be collapsed merely because their JSON is equal.

For the same key and digest, return the recorded result. A key reused with different input is an error. The receipt is committed with the command, not in a later best-effort write.

Version one guarantees replay across reconnect/restart within the retry namespace validity window (initial policy: 24 hours). Receipts remain until that namespace expires. Persist or authenticate the namespace and its fixed expiry so garbage collection cannot make an old operation look new. Reconnecting can renew a channel but cannot extend an old retry namespace or transplant its pending operations into a new one. Expired/resurrected namespaces are refused as stale rather than silently converted into new work. Backup restore creates a new generation; old keys and preview tokens are invalid.

Storage conflicts can retry from a fresh snapshot with bounded backoff. A caller's stale `expected_revision` is not automatically rebased into a destructive overwrite. Re-read and surface a domain conflict. Do not retry an I/O error as though it were an ordinary optimistic conflict.

Native commands support expected revisions. The legacy adapter cannot add a required `expected_revision` to Lemma's schemas: it retains upstream observable semantics and serializes/revalidates internally. It cannot infer an agent's stale intent from an earlier unrelated read.

### 5.3 Atomic operation catalog

| Command | Canonical write set in one operation | Principal invariant |
|---|---|---|
| Add memory | memory, alias, project membership, session link if applicable, receipt, pending projection | No orphan identity or acknowledged-but-unindexable missing source |
| Update memory | memory revision, changed indexes/membership, evidence state, receipt, pending work | No lost fields or stale precondition success |
| Feedback | feedback record, observable counters/confidence, receipt | One logical feedback produces one effect |
| Relate/unrelate | edge, adjacency/index state, receipt | Both traversal directions reflect the same graph state |
| Merge | resulting memory, source lifecycle/redirects, required edges/references, receipt, pending work | No visible half-merge |
| Forget/invalidate | lifecycle/tombstone, affected graph policy, receipt, projection invalidation | Deleted knowledge cannot be resurrected by a delayed worker |
| Session end | terminal session state, required guide outcomes and attempt/history state, receipt | Exactly one permitted terminal transition |
| Guide merge/forget | guide state, dependencies/links and preserved history, receipt | No silently dangling actionable guide references |

Port the exact legacy distinctions between forget, invalidate, merge and consolidate. The table specifies transaction boundaries, not a replacement for those behaviors.

## 6. Domain identity, scope and graph

### 6.1 IDs and versions

Use 128-bit internal identifiers. Compact Lemma IDs are external aliases with an atomic uniqueness check and collision retry; never truncate UUIDs and assume uniqueness. Preserve imported aliases and numeric suggestion IDs through typed mappings.

Separate `entity_revision` from `document_revision` and `eligibility_revision`. Content changes advance document revision. Scope/lifecycle changes affect eligibility. Read statistics must not invalidate an otherwise valid vector. UUIDv7 is useful as an identity, but its time ordering is not commit ordering.

A search identity is at least `(store_generation, memory_id, document_revision, model_fingerprint, chunk_id)`. Do not use a Lance physical row address as a persistent domain ID.

### 6.2 Required records

Keep one canonical record for each memory, guide, session, attempt, feedback event, suggestion, project and explicit relationship. Preserve provenance/evidence, archives, aliases, guide contexts/learnings/dependencies, source-memory links, history and refinement links where upstream supports them. Unknown legacy fields are preserved in an import envelope and reported, not silently discarded. [S03]

`quality_score` remains optional. An unknown score is not zero. `source` in the legacy response remains the upstream enum; import provenance is a separate field, not a new unsupported `source="import"` value.

Store raw imported timestamp text when normalization would lose precision or meaning. Use UTC instants internally and a frozen test clock. Match legacy date boundaries in the adapter, including date-only fields and absent/null behavior.

### 6.3 Projects

The frontend supplies its working-directory/root context. The daemon must never use its own CWD to classify a client's memory.

Native project identity is stable and distinct from a directory basename. Legacy project names retain the pinned normalization/omission/global rules. Do not silently merge unrelated native projects both named `app`, nor globally lowercase case-sensitive filesystem paths.

The effective scope is computed once from the selected profile and request. Project plus global inheritance is deliberate, not "all projects." Direct-ID access, bulk IDs, guides, graph expansion and context injection must obey the defined scope and explicit cross-project modes.

This prevents accidental mixing. It is not a strong isolation boundary against another process with the same OS UID and filesystem access; a future multi-user service would require a different security model.

### 6.4 Graph invariants

Explicit edges represent claims or history, never automatic truth inferred from cosine similarity. Store one canonical edge per logical relation and render reverse views when required.

Supersession is directed: `new -> supersedes -> old`; the inverse is derived. Contradiction and relatedness are symmetric. The legacy adapter must preserve the pinned upstream treatment of `supports`, including its reverse relation behavior, rather than silently assigning a different meaning. [S03, S05]

Enforce endpoint existence/type and logical uniqueness for native writes. Keep imported unresolved references as documented legacy references where refusing them would lose data; they are not traversable live edges until resolved.

Reject self-supersession and new supersession cycles. A cycle check must be concurrency-safe: reading a path outside a transaction and checking only two endpoint revisions is insufficient against simultaneous cycle-forming edges. Use transactional predicate dependencies or a graph-mutation lock covering the entire affected graph scope, held through final validation and durable atomic publication. Do not use an unfenced connected-component lock when concurrent edges can merge components. If the limit prevents deciding safely, reject with a specific error or require maintenance mode.

Invalidation, archival, supersession and hard deletion are distinct. Normal recall excludes invalidated/archived/deleted records according to the profile. Historical direct reads and explicit history modes may expose preserved material with its state clearly shown.

## 7. Daemon and session model

### 7.1 Ownership and transport

One binary supplies `ltmrs` (MCP frontend), `ltmrs daemon --foreground`, CLI commands and the optional visualizer. Do not put a second MCP dispatcher inside the daemon: rmcp terminates stdio in the frontend, which sends typed domain requests over IPC.

The private IPC envelope contains protocol version, store identity/generation, frontend identity, channel identity, request/operation ID, session binding, deadline and a typed body. Use length-prefixed UTF-8 JSON initially. Large library/export responses stream in bounded frames. Do not apply the frame limit as silent result truncation.

On Linux use a user-owned 0700 runtime directory, a 0600 socket and peer-credential checks. Singleton startup is protected by an OS lock held for daemon lifetime, not just a PID file. Verify generation/protocol in the handshake; handle concurrent startup and stale sockets safely. An incompatible live daemon is not killed or migrated underneath active clients.

Persistent data is separate from runtime sockets and rebuildable cache. Resolve the lock/socket by store identity so an explicit portable store does not collide with the default store.

### 7.2 Session binding

Distinguish OS user, MCP frontend instance, frontend channel, logical agent session, tool request and durable operation. None is an alias for the others.

Legacy `session_start`, `session_attempt` and `session_end` use the current session **for that frontend channel**. Session-less calls use that channel's virtual-session logic, matching the pinned upstream rules. Multiple independent MCP frontends therefore remain isolated.

Two subagents multiplexed by a host onto the exact same legacy channel cannot be distinguished reliably when their tool schemas supply no identity. The service must not guess from task text. They either intentionally share that channel's legacy session or use native explicit session bindings/host integration. This is a compatibility boundary to document and test, not a storage-engine problem.

On reconnect, restore only a verified channel binding. An unexpected disconnect does not automatically end another channel's session. A daemon restart preserves durable history; lease expiration and the profile's virtual-session rules determine abandonment.

### 7.3 Scheduling

Keep synchronous KV transactions and CPU inference off Tokio core workers. A concrete embedding service owns a bounded worker queue; the model may be a synchronous `&mut self` adapter. An async trait spelling is not itself a working dynamic plugin system.

Enforce limits for clients, per-client queued work, in-flight storage operations, snapshot lifetime, inference tokens, batch size, response bytes and maintenance jobs. Fairness must prevent an indexing rebuild from starving interactive recall. Backpressure is visible as a retryable busy/error state, not unbounded allocation.

Cancellation before publication may abort. Cancellation after publication must not hide or undo the durable receipt. Index tasks may continue for committed writes; canceled uncommitted requests must not create memories.

## 8. Derived search lifecycle

### 8.1 Canonical versus derived state

Canonical state must remain usable if Lance search indexes, model files or an embedding worker fail. Derived state includes rendered search text, chunks, vectors, full-text/vector indexes and their generation metadata. It is reproducible from canonical state plus the pinned model/renderer recipe.

In the dual-engine design, duplication is limited to fields required for retrieval. Do not mirror all session graphs or volatile counters into Lance. In the Lance-only design the same logical distinction applies even if everything resides in one engine.

### 8.2 Pending work, not an unsafe sorted-ID cursor

For Candidate B, each canonical transaction sets or updates a durable pending record keyed by `(entity_id, target_generation)` with desired document/eligibility revisions and operation type. Coalescing means the latest desired state wins. Delete/invalidate creates a durable tombstone/invalidating job.

A projector:

1. Reads pending work without holding a write transaction during inference.
2. Reads canonical state and captures its versions/fingerprint.
3. Renders and embeds only the fields that changed.
4. Acquires the per-entity or table publication guard and revalidates canonical versions and target generation inside that guarded path.
5. Publishes an idempotent Lance upsert/delete without regressing an already-published document revision; concurrent canonical changes are caught by hydration and the pending-work check below.
6. Compare-and-clears only the pending revision it actually published.

If a newer mutation arrives, compare-and-clear fails and leaves work pending. If the process dies after Lance publication but before acknowledgement, replay is safe. No checkpoint advances simply to the lexicographically largest UUID seen.

A generation has its own projection watermark/status. Model changes never compare raw entity revisions across different vector spaces.

### 8.3 Freshness and reads

Canonical write success does not mean its embedding already exists. Return the memory's durable identity immediately after canonical commit, with index status conveyed through the profile's supported response surface.

Direct get/list operations use canonical state. Query-aware recall distinguishes `complete`, `lexical_only`, `index_pending` and `partial_budget` internally. Legacy output uses a tested compatible warning/text channel rather than inventing required JSON fields.

A query can wait for its channel's known pending revisions up to a bounded deadline. If they remain pending, do not silently claim a complete current semantic search. Strong search modes wait or return a specific readiness error. Native best-effort modes may return labeled partial results.

For every candidate, hydrate current canonical metadata from a consistent snapshot. Reject wrong-generation, obsolete-content, deleted, out-of-scope and ineligible hits. A record changing only confidence need not be re-embedded, but confidence eligibility must be checked.

Lance's documented scan of unindexed appended rows addresses **rows already in Lance**; it does not repair writes that have not reached the projection. Those are separate lag mechanisms. [S13]

## 9. Embedding contracts

Candle owns inference; Lance receives vectors. Do not enable Lance's built-in model wrapper unless its exact behavior is the selected tested recipe. The service owns model downloads, masking, pooling, prompts and model migration.

### 9.1 First model

`intfloat/multilingual-e5-small` remains the initial candidate, not a certified working ltmrs adapter. Its inspected config declares **BertModel / model_type=bert**, hidden size 384, and **XLMRobertaTokenizer**. The small model must not be assumed identical to all other multilingual-E5 sizes or to vanilla WordPiece BERT. [S16]

The reference recipe uses `query: ` and `passage: `, including non-English input, a 512-token limit, attention-mask-aware mean pooling and L2 normalization. Validate padding IDs/masks from the actual tokenizer artifacts rather than a guessed generic default. [S17]

No model family is advertised until architecture loading and reference-vector tests pass. Changing `model_type` alone is insufficient: model-specific pooling, projection heads, instructions or adapters can change semantics. Qwen/Nomic/Jina/ModernBERT are later evaluated adapters, not an untested supported-model list.

### 9.2 Model manifest

Record model ID, immutable revision, weight/tokenizer/config digests, license record, architecture adapter version, inference dtype, output dimension, normalization, pooling, query/document recipe and truncation/chunking policy. Never mix vectors merely because both have 384 dimensions.

Downloads are explicit, authenticated through trusted configuration where necessary, checksum-verified and atomically cached. Offline mode must not attempt downloads. Do not execute downloaded Python/remote model code or load untrusted pickle files.

### 9.3 Oversized memories

A canonical memory remains one node and one external ID. Its retrieval projection may contain several deterministic chunks when the actual tokenizer would exceed the model window. Keep title/type/context as a bounded prefix and split at stable paragraph/line boundaries. Store offsets and the renderer/chunker version.

Collapse chunk hits by parent before fusion and context budgeting. Use the best matching chunk or another documented bounded aggregation, not an unbounded sum favoring long memories. Return canonical identity plus exact matched spans; full detail remains a separate read.

Input truncation is tested and disclosed, never justified by "2000 characters is approximately 512 tokens."

## 10. Retrieval and context assembly

### 10.1 Pipeline

```text
Resolve profile and scope
  -> direct-ID / empty-query routing
  -> lexical candidates + semantic candidates
  -> canonical eligibility/revision validation
  -> collapse chunks by parent memory
  -> rank fusion
  -> resolve authoritative supersession / conflict context
  -> bounded graph enrichment
  -> calibrated relevance and priority
  -> bundle-aware diversification
  -> context budget and explanation
```

Each stage exposes deterministic diagnostics for tests. No absolute cosine score is called a truth/confidence probability.

### 10.2 Scope and candidate completeness

Use the same project/global, type, date and lifecycle predicate for both retrieval legs and graph expansion. Exact-identifier handling is distinct from stemming/tokenization. Include tests for underscores, punctuation, paths, flags, case and French accents.

Frequently mutable eligibility such as `minConfidence` requires a correct plan: pass an allowed-ID set from canonical state where practical, or overfetch/revalidate with bounded backfill and report limits. Filtering only the first ten hits and returning two as "the best two" is not sufficient.

An empty query follows upstream list/priority behavior. A nonempty no-match query may legitimately return nothing. Establish no-answer thresholds using held-out labels, not a universal cosine threshold copied across models.

### 10.3 Scoring correction

The old formula has a scale problem. For two RRF lists with k=60 and one-based rank, maximum raw RRF is `2/61`, about 0.03279. Adding at most 0.05 priority gives about 0.08279, while a cosine similarity of 0.8 creates an MMR penalty of 0.24 at lambda=0.70. Diversity can therefore overwhelm relevance by construction.

Retain the upstream scoring formula as a **reference oracle/baseline**, not an untouchable native ranking policy. [S04]

Initial native candidate scoring, to be calibrated:

```text
RRF(d) = sum_j w_j / (60 + rank_j(d))
R(d)   = RRF(d) / (sum_j (w_j / 61))
G(d)   = maximum bounded eligible path contribution from selected seeds
P(d)   = clamped priority in [0,1]
S(d)   = 0.90 * max(R(d), 0.80 * G(d)) + 0.10 * P(d)
MMR(d) = lambda * S(d) - (1-lambda) * max_selected max(0, cosine(d,s))
```

The denominator uses active rankers and explicit weights; all candidate components have a defined range. The sample coefficients are tuning seeds, not performance claims. Calibrate on development labels and freeze before held-out evaluation.

Graph contribution uses bounded paths and deduplicated sources; high-degree nodes do not accumulate unlimited bonus. Exact-ID lookups bypass this ranker. Missing vectors use a documented lexical diversification fallback, never a zero-vector placeholder.

### 10.4 Corrections and contradictions

A known valid superseding memory is not merely a slightly higher-scored neighbor. Redirect actionable context to the current applicable record, preserve lineage in explanations and exclude obsolete advice from the primary answer by default. If the replacement is out of scope, do not leak it or silently re-promote the obsolete instruction.

Two competing active replacements or explicit contradictions form an unresolved conflict bundle. Preserve both required sides when within the allowed scope and budget. MMR must not delete the contradiction because the sentences are semantically similar. If a complete warning cannot fit, emit a concise conflict notice with IDs rather than silently showing only one claim.

### 10.5 Injection and token budgets

`tools/list` has no user query. Its context is project/channel/profile-scoped priority recall, not task-aware dense retrieval. Query-aware recall occurs through calls such as `memory_read`, `semantic_search` and session startup context available in the pinned schema. [S02, S07]

Dynamic content is serialized as untrusted memory data with provenance/state boundaries, not promoted into privileged instructions. Tool schema descriptions must not create cross-channel caches or enormous prompt prefixes. Updates use the pinned MCP notification behavior where supported, with debounce and host testing. Installation or notification is not a guarantee of model attention.

Budget the serialized output, including labels, warnings and wrappers. A known target tokenizer gives token-accurate accounting; otherwise use a documented conservative byte/character budget, not an invented exact token count. Select whole coherent bundles or explicit summaries supplied by deterministic formatting; do not silently fabricate abstractive summaries.

## 11. Lemma compatibility and intentional enhancements

### 11.1 What is preserved

The target is the full supported Lemma 0.21.0 callable and workflow surface: all tools, input/output schemas, required fields, defaults, annotations, error envelopes, text/JSON format choices, pagination, project rules, read side effects, guide/session behavior, instructions, injection, CLI aliases, skill workflow and data interchange. Source types include substantially more state than the old simplified `Memory` struct. [S02, S03]

A `--compat=lemma` adapter uses the frozen schemas directly rather than trusting generated Rust schemas to match all nullability/default/camelCase details. Internal DTOs and native extensions are separate. Never append new mandatory fields to old tool signatures.

### 11.2 What must be reported separately

Dense semantic search and GraphRAG intentionally change ranking and score interpretation. Therefore the accurate release claim is **complete supported Lemma API/workflow surface with documented retrieval enhancements**, not byte-identical behavioral equivalence.

The deviation ledger records each enhanced result ordering, score definition, security hardening and concurrency extension. A consumer requiring exact upstream TF-IDF ordering must use a separately implemented/tested reference profile or the upstream server; do not claim the enhanced profile satisfies that requirement.

Client-specific prefixes are normally controlled by the host configuration, not merely `serverInfo.name`. Retain the client's configured server key when documenting a drop-in swap. Do not spoof the upstream version: identify ltmrs and its compatibility target honestly, even when a compatibility display name is used.

### 11.3 Skills and CLI

Ship a managed ltmrs skill and an explicit legacy-skill installation path. Record ownership and hash/version; write atomically; preserve user modifications and foreign/upstream files unless replacement is explicitly approved. Never have two startup installers overwrite each other.

Treat installation, host discovery, model activation and workflow behavior as four separate outcomes. Test host versions explicitly. No promise that every host reads `~/.agents/skills` identically. The native skill may describe multilingual recall; the legacy skill's English-only instruction must be preserved or listed as an intentional workflow enhancement. [S06]

Match the known legacy CLI aliases including visualizer foreground/port behavior where that surface is claimed. A visualizer cannot be counted as compatible while its CLI flag only prints "coming soon."

### 11.4 Data interchange

A `.ltmrs-backup` is not a `.lemma-backup`. The compatibility release includes an actual reader for supported Lemma backup/schema versions and an explicit loss-accounted legacy export path. Native-only metadata is preserved in native exports and identified in the legacy export report. Do not rename the extension and call it interoperable.

## 12. Safety, backup and lifecycle

### 12.1 Privacy and integrity

The default privacy path scans and redacts detected secrets before canonical persistence or embedding. Diagnostics must not log raw sensitive content. Detection is best effort, with testable patterns and no claim of perfect secret recognition.

The pinned Lemma `memory_add` schema includes `confirm=true` to store a fragment verbatim despite secret detection. The compatibility adapter must handle this explicitly: an approved legacy policy preserves the override; a stricter policy rejects it with a clear policy error and records a compatibility deviation. It must not silently redact while reporting an unchanged successful save. Keep verbatim-sensitive storage separate from permission to send content to an external provider or include it in diagnostics. Local embedding and export behavior must follow a documented privacy policy, with any withheld semantic coverage reported. [S02] Treat memory and skill text as potentially hostile instructions. Do not execute commands, follow arbitrary URLs or open arbitrary filesystem paths from stored content.

Evidence freshness checks are opt-in, root-confined, size-limited regular-file reads. Protect against path traversal, symlink replacement, special files and stale evidence references. Queries use typed/bound predicates or a narrowly audited escaping layer, never raw interpolated user syntax.

### 12.2 Consistent backups

Create a logical export from one consistent canonical snapshot. Include every canonical entity, alias, archive, relationship and required configuration meaning. Exclude model weights and derived indexes by default; source privacy warnings remain explicit. Checksums detect corruption, not malicious authenticity.

The backup contains a versioned manifest, entity counts, per-file digests, schema/profile versions and a complete record inventory. A staged file is flushed and atomically renamed only after verification. Snapshot lifetime must be bounded; large backups may require a coordinated checkpoint rather than holding an unbounded live snapshot.

### 12.3 Restore protocol

Restore is a maintenance operation:

1. Inspect the archive with bounded parsing and path-safe extraction.
2. Validate format, checksums, schema compatibility and relationships.
3. Preview replacement, conflicts, active leases and unsupported fields.
4. Issue a single-use, short-lived token bound to channel, store identity/generation, canonical state and archive digest; use the legacy expiry where required.
5. After explicit confirmation, revalidate the exact bytes/state, block new writes, drain accepted writes and invalidate unsafe previews.
6. Produce and verify a safety backup.
7. Restore into a new staged generation; validate it independently.
8. Durably publish the active-generation pointer using the tested platform protocol.
9. Invalidate old caches, model jobs, projection work and stale session bindings; reopen readers against the new generation.
10. Rebuild derived state and report readiness, not fictitious immediate semantic completeness.

If publication is interrupted, startup selects a verified complete old or new generation. Never destructively overwrite the sole live copy and hope the engine transaction spans a directory replacement.

### 12.4 Import

Source `.lemma/` can include SQLite/WAL state and legacy sidecars. A read-only flag alone does not create a coherent copy. Use a supported SQLite snapshot/backup path or require an offline source and verify its coherency. Do not run the upstream migrator against the original store.

Import into staging. Preserve all supported fields/IDs, record every unsupported or malformed item, validate counts and relationships, then publish. Embedding generation is subsequent derived work. One-off SQLite access may be an explicitly isolated C-based importer/helper; it is not a C++ canonical database dependency. The minimal core build need not include it.

## 13. Required decisions and handoff

| Decision | Current state | Evidence that closes it |
|---|---|---|
| AD-01 Canonical backend A or B | OPEN, A tested first | WP-02 hard gates and Part III traces |
| AD-02 Exact crate/feature/toolchain lock | OPEN | Reproducible local CPU build and dependency report |
| AD-03 Legacy contract baseline | Version/commit identified; wire capture OPEN | WP-00 generated snapshot and upstream transcripts |
| AD-04 Default Candle recipe | E5-small candidate | Reference equivalence and retrieval quality suite |
| AD-05 Score/abstention thresholds | Initial normalized design only | Development tuning plus frozen held-out evaluation |
| AD-06 FTS freshness/null-vector behavior | OPEN for pinned Lance build | Local create/update/delete/search capability probes |
| AD-07 Host/platform support | Linux-first target; no runtime certification yet | Versioned host/OS matrix |

No runtime code should depend on an unclosed decision without a failing test and explicit experimental label.

## 14. Requirements and traceability

| Requirement | Contract | Work package | Tests |
|---|---|---|---|
| RQ-01: Atomic domain commands | A supported multi-entity mutation and its deduplication receipt publish atomically; canonical readers cannot see a partial operation. | WP-02 | T-STORE-01, T-STORE-02, T-REC-01 |
| RQ-02: Hard decision gates | A candidate failing correctness, offline packaging or license gates is ineligible regardless of throughput. | WP-02 | T-GATE-01 |
| RQ-03: Pinned capabilities | All adopted dependency features are identified by exact release/source and exercised through the selected local Rust API. | WP-00 | T-BUILD-01, T-STORE-03 |
| RQ-04: Revision and uniqueness enforcement | Conditional updates, create-if-absent, edge uniqueness and typed alias allocation are atomic under concurrency. | WP-03 | T-CONC-01, T-CONC-02, T-GRAPH-01 |
| RQ-05: Session isolation | Every call has a stable frontend channel and authorized session context; legacy implicit session state never crosses channels. | WP-04 | T-SESS-01, T-SESS-02 |
| RQ-06: Retry-safe effects | Retried logical commands within the documented replay window have one canonical effect; ambiguous outcomes are not blindly replayed as new work. | WP-03 | T-CONC-03, T-REC-02 |
| RQ-07: Projection convergence | Pending work is durable, versioned and acknowledged only by compare-and-clear after projection publication. | WP-05 | T-PROJ-01, T-PROJ-02 |
| RQ-08: Freshness contracts | Direct reads observe acknowledged writes; search never silently represents stale revisions or unavailable embeddings as complete current semantic results. | WP-05 | T-PROJ-03, T-SEARCH-01 |
| RQ-09: Model correctness | The selected Candle adapter matches a pinned reference recipe within documented numerical and retrieval tolerances. | WP-06 | T-EMB-01, T-EMB-02 |
| RQ-10: Long-memory handling | Long input produces explicit bounded chunking or a visible coverage limitation, never silent semantic loss. | WP-06 | T-EMB-03 |
| RQ-11: Calibrated ranking | Native score components share a documented scale; legacy scoring and enhanced scoring have separate evidence. | WP-07 | T-RANK-01, T-RANK-02 |
| RQ-12: Graph semantics | Graph mutations enforce endpoint/direction/uniqueness/cycle policy, and retrieval preserves corrections and unresolved conflicts. | WP-03 | T-GRAPH-01, T-GRAPH-02, T-RANK-03 |
| RQ-13: Scope enforcement | Scope, lifecycle, date/type filters and confidence eligibility apply to lexical, dense, graph, direct reads and generated context. | WP-07 | T-SCOPE-01, T-SCOPE-02 |
| RQ-14: Relevance and abstention | Exact identifiers remain retrievable; no-match queries may return no applicable memory; zero scores and NaNs do not enter rank fusion. | WP-07 | T-SEARCH-02, T-RANK-04 |
| RQ-15: Complete compatibility surface | Every pinned Lemma tool, schema, behavior, instruction path, command and applicable skill contract is tested or explicitly marked as a named deviation. | WP-08 | T-MCP-01, T-MCP-02, T-MCP-03, T-CLI-01 |
| RQ-16: Loss-accounted data model | Known fields, nullability, evidence, archives, guide dependencies and legacy history survive serialization and interchange. | WP-01 | T-DATA-01, T-IMPORT-01 |
| RQ-17: Observable bookkeeping | Read/access/feedback side effects visible in the compatibility contract are not silently made eventually consistent. | WP-08 | T-MCP-04 |
| RQ-18: Durability qualification | A durable ACK includes required storage barriers; process-crash and power-loss qualifications remain distinct. | WP-02 | T-REC-01, T-REC-03, T-BENCH-01 |
| RQ-19: Safe recovery and restore | A verified snapshot is restored through a coordinated generation switch; confirmation is digest/state/channel-bound and single use. | WP-11 | T-BACKUP-01, T-BACKUP-02, T-BACKUP-03 |
| RQ-20: Local security boundary | Only approved same-user frontends connect; inputs, paths, resource limits and dynamic context are treated as untrusted data. | WP-04 | T-SEC-01, T-SEC-02, T-SEC-03 |
| RQ-21: Auditable distribution | The CPU release requires no Python/Node/ONNX or C++ database engine; native exceptions and model rights are explicitly audited. | WP-00 | T-BUILD-01, T-SEC-04 |
| RQ-22: Bounded concurrency | Queues, snapshots, inference, maintenance and per-client load have bounded resources and documented overload behavior. | WP-04 | T-CONC-04, T-BENCH-02 |
| RQ-23: Reproducible benchmarks | Performance comparisons use equivalent guarantees, realistic traces, declared hardware and reproducible raw measurements. | WP-12 | T-BENCH-01, T-BENCH-02, T-BENCH-03 |
| RQ-24: Measured retrieval quality | Quality is judged at fixed result/context budgets on held-out labels, including multilingual, conflict, stale and no-answer cases. | WP-12 | T-QUALITY-01, T-QUALITY-02 |
| RQ-25: Skill and host conformance | Skill ownership, installation, discovery and workflow activation are distinct tests; foreign assets remain untouched. | WP-10 | T-SKILL-01, T-HOST-01 |
| RQ-26: Coherent import | The importer reads a coherent supported source snapshot, handles references explicitly, reports every loss, and does not modify the original source. | WP-11 | T-IMPORT-01, T-IMPORT-02 |
| RQ-27: Incremental delivery | Each slice has an end-to-end demonstration and regression evidence; no placeholder handler counts as implemented compatibility. | WP-13 | T-RELEASE-01 |
| RQ-28: Evidence discipline | Planned, inspected, executed, passed, failed and waived statuses are never conflated; unrun benchmarks carry no invented values. | WP-13 | T-GATE-01, T-RELEASE-01 |


The same mapping is machine-readable in [traceability.json](traceability.json). Part II assigns work; Part III defines assertions and release evidence. The source register distinguishes inspected upstream facts from ltmrs design decisions.
