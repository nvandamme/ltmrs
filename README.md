# ltmrs — Long Term Memory RS

**ltmrs** (the "long timers" pun) is a local memory service for LLM coding agents.
It persists facts, lessons, guides, attempts and knowledge relations, and recalls
them by identifier or meaning — over MCP, from any number of clients, on one
machine, with no cloud calls.

The agent decides what to save and how to reason. ltmrs stores explicit findings
and concise attempt summaries; it does not capture hidden chain-of-thought and
never calls a remote model.

## Status

**Design and planning phase. No backend, model or conformance test has been
executed yet.** This repository currently contains:

- the reviewed implementation specification and test plan in [`plans/`](plans/),
- an empty Cargo skeleton,
- the repository governance files.

Nothing in `plans/` is a passing test report or a wire capture. All conformance
matrix cells start at `not_run` by design.

## Compatibility target

ltmrs targets the complete supported tool/workflow surface of
[Lemma](https://github.com/xenitV1/lemma) `0.21.0`
(commit `d30a816632d0bc5d92907cbc51c1dc1010111986`), including its data
interchange, CLI aliases and skill workflow.

Dense semantic search and graph-aware context are **intentional enhancements**.
The honest release claim is therefore: *complete supported Lemma
API/workflow surface with documented retrieval enhancements*, not byte-identical
behavioral equivalence. Every deviation is tracked in an explicit ledger.

## Architecture (planned)

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

- One binary: MCP stdio frontends, `ltmrs daemon`, CLI and optional visualizer.
- One daemon owns one physical store; multiple frontends share it.
- Canonical knowledge and derived search state are logically separated.
- Embeddings are computed locally with Candle; retrieval with LanceDB.
- Backend candidates under test: **A** LanceDB-only, **B** Fjall + LanceDB.
  The decision (AD-01) is open and gated on hard correctness/atomicity tests,
  not on feature lists or throughput scores.

## Constraints

- Rust-native core. The production CPU profile must not require Node, Python,
  ONNX Runtime or a C++ database engine (feature-resolved audit, not a blanket
  "100% Rust" claim).
- Local operation only. No remote database, cloud sync or hidden model calls.
- OSI-approved dependencies; model weights and tokenizer licenses audited
  separately.
- First supported target: local Linux x86_64 CPU. CUDA is an optional,
  independently tested feature.

## Plans

The specification is the source of truth for implementation:

| Document | Purpose |
|---|---|
| [Part I — Design and concepts](plans/01_design_and_concepts.md) | Normative contracts: consistency, sessions, graph, projections, models, retrieval, compatibility, safety, requirements RQ-01…RQ-28 |
| [Part II — Implementation guide](plans/02_implementation_guide.md) | Ordered work packages WP-00…WP-13, delivery slices S0…S8, repository layout |
| [Part III — Quality, tests, benchmarks, conformance](plans/03_quality_tests_benchmarks_conformance.md) | Hard gates, test catalog, recovery/security suites, benchmark strategy, release checklist |
| [traceability.json](plans/traceability.json) | Machine-readable requirements → work packages → tests mapping |
| [conformance_matrix.json](plans/conformance_matrix.json) | Per-tool conformance inventory (all `not_run` until executed) |

Implementation starts with **WP-00** (freeze upstream baseline, resolve the CPU
dependency lock) and **WP-01** (domain model + reference interpreter). Do not
implement the full tool surface against an unproven storage transaction
assumption.

## Building

```bash
cargo build      # builds the current skeleton
cargo run
```

The planned repository layout, toolchain pin and development runners are
defined in [Part II, §3 and §20](plans/02_implementation_guide.md).

## License

MIT OR Apache-2.0 — see [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
Upstream notices for translated MIT Lemma code are retained per the plan.
