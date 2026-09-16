# ltmrs — Dependency & Native-Code Audit (WP-00)

**Status:** resolved + compile-validated (cargo check passes)  
**Date:** 2026-09-16  
**Toolchain:** Rust 1.96.0 (x86_64-unknown-linux-gnu)  
**Evidence:** `Cargo.lock` (590 packages), `cargo check` exit 0

This is a feature-resolved audit, not a blanket "100% Rust" claim. It records
which transitive native dependencies the CPU release pulls in and why each is
acceptable or a concern, per Part I §2.2 and RQ-21.

## Resolved candidate set

| Crate | Version | Role | Native code |
|---|---|---|---|
| rmcp | 3.4.0 | MCP stdio frontend + daemon dispatcher | none (features: server, transport-async-rw) |
| lancedb | 0.38.0 | Candidate A canonical + retrieval | via lance (below) |
| lance | 11.0.0 | Candidate A engine | zstd-sys (C); protobuf (build-time) |
| fjall | 3.1.10 | Candidate B canonical KV | none (pure Rust) |
| candle-core | 0.11.0 | embeddings (CPU) | onig_sys (C, vendored) |
| candle-nn | 0.11.0 | embeddings (CPU) | none direct |
| candle-transformers | 0.11.0 | model architectures | none direct |
| tokenizers | 0.22.2 | tokenizer | onig_sys (C, via tokenizers) |

## Native / system dependencies (unavoidable or chosen)

| Dependency | Type | Source | Assessment |
|---|---|---|---|
| zstd-sys | C (compression) | lance → arrow-ipc | Acceptable: compression lib, not a DB engine. |
| onig_sys | C (regex, vendored) | candle-core → tokenizers | Acceptable: vendored, no system pkg. |
| ring | C (crypto) | rustls ← quinn ← reqwest ← lancedb/remote | Concern: only via the `remote` feature (see below). |
| protoc | build tool | lance build.rs (prost_build) | Build prerequisite on PATH (v36 present). |

## Findings

### F-01: lancedb 0.38.0 requires the `remote` feature to compile (BLOCKER-level constraint)

`src/lib.rs` declares `pub mod job;` unconditionally, but the `Error::Http`
variant used by `job.rs` is gated behind `#[cfg(feature = "remote")]`. As a
result, lancedb 0.38.0 **does not compile with `default-features = false`** —
it fails with `no variant named Http found for enum Error`.

- This is a genuine upstream feature-gating defect (matches RV-03: main-branch
  / feature / namespace APIs conflated).
- 0.38.0 is the latest published version; no patch fixes it.
- **Resolution:** enable `features = ["remote"]`. This pulls
  `reqwest` + `http` + `urlencoding` + `lance-namespace-impls/rest`. These are
  for remote namespace access, **unused in ltmrs local mode**, but compiled in.

**Impact on RQ-21:** the CPU release transitively includes an HTTP client
(rustls/quinn/ring) solely to satisfy lancedb's compile requirement. This must
be disclosed in the release audit. It is not a C++ database engine and not
Node/Python/ONNX, so it does not violate the core constraint — but it is a
non-Rust transitive dependency that must be named, not hidden.

### F-02: candle version conflict avoided

lancedb's optional `sentence-transformers` feature depends on **candle 0.9.1**,
which is semver-incompatible with the desired **candle 0.11.0** (0.x semver).
Enabling it would compile two candle versions (type-mismatch risk).

- **Resolution:** do NOT enable `sentence-transformers`. Depend on candle
  0.11.0 directly and supply embeddings to lancedb via its embedding-function
  API (per Part I §9: "Candle owns inference; Lance receives vectors").
- Verified: only one `candle-core` (0.11.0) in Cargo.lock.

### F-05: tokenizers version alignment

candle-core 0.11.0 depends on **tokenizers 0.22.2**. An initial explicit
`tokenizers = "0.21"` pin created a duplicate (0.21.4 + 0.22.2) in the graph.
- **Resolution:** align the explicit `tokenizers` to 0.22.2 so the embedding
  pipeline uses a single tokenizer version. Verified: one tokenizers in lock.

### F-03: GPU features kept off

No `cuda`/`cudnn`/`nccl`/`metal`/`mkl`/`accelerate` features enabled on any
candle crate. `candle-flash-attn` (CUDA-only, uses cudaforge) is excluded.
CPU F32 is the baseline (per Part I §9, WP-06).

### F-04: cloud/object-store features kept off

lancedb and lance use `default-features = false`, so no aws/azure/gcs/oss
object-store or cloud credentials are pulled in. Verified: no `object-store`,
`opendal`, `aws-config`, etc. in Cargo.lock.

## Build prerequisites (documented, not bundled)

- C compiler (cc/gcc) — required by zstd-sys, onig_sys, ring.
- protoc on PATH (or `PROTOC` env) — required by lance build.rs.
  - Alternative: enable lance's `protoc` feature (builds protobuf-src C++),
    more self-contained but slower. Chosen: system protoc for dev.

## Toolchain

- Rust 1.96.0 pinned via `rust-toolchain.toml`.
- AD-02 (exact toolchain lock) closes when the reproducible CPU build is
  validated end-to-end; this audit is the input.

## Model licenses (deferred)

No embedding model is selected yet (AD-04 open). The initial candidate is
`intfloat/multilingual-e5-small` (BertModel config + XLM-RoBERTa tokenizer,
384-d). Its weight/tokenizer license must be audited separately from code
licenses when the model is pinned (WP-06). Not claimed as a supported model
until T-EMB-01/02 pass. This audit does not cover model-weight redistribution
rights.

## Open questions for WP-02

1. Does the `remote` feature's reqwest stack affect the offline-install gate
   (T-BUILD-01)? It compiles but must be confirmed unused at local runtime.
2. Should we switch to lance's `protoc` feature for a fully self-contained
   build, accepting the C++ protobuf compile cost?
3. Is there a lance/lancedb fork or upcoming release that fixes F-01?
