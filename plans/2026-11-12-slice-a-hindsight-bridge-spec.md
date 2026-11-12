# Slice A — Hindsight Bridge (B1): Spec

## Goal

Live dual-write ltmrs→Hindsight `retain` + scoped `recall`/`reflect` for cross-session coverage. ltmrs stays authoritative: every canonical commit succeeds/fails identically with the bridge on or off; Hindsight is best-effort additive only.

## Non-goals

No change to canonical receipts, admission, budgets, ranking (RQ-11), or embedding/search pipelines. No auto-`reflect` in B1 (manual-only; auto-reflect is B2). No upstream Hindsight version pin in this spec (Phase 0 freezes wire shapes from the omp precedent).

## Architecture

New code lives in `crates/interchange` (owns external bridges per DAG `domain → service/compat → embeddings → search → interchange → daemon → frontend`). Daemon holds an optional bridge handle and calls it at fan-out points; no HTTP code in `daemon`, no new crate, no shims/re-exports.

Constraint: `crates/daemon/src/server/connection.rs:198-199` dispatches via `spawn_blocking` (sync context). Bridge I/O MUST NOT block tool ACK: dual-write runs post-commit fire-and-forget via a bounded background worker, never on the ACK path. Reuse pinned `reqwest 0.12 rustls-tls` (already in `embeddings`/`frontend`).

Spec lives in `plans/` (tracked). Precedent (unverified in-tree): omp `packages/coding-agent/src/hindsight/client.ts` — hand-rolled fetch, `retain`/`recall`/`reflect`, `document_id` upserts; Phase 0 re-verifies on the omp host before freezing wire shapes.

## Global constraints

- ltmrs authoritative: bridge on/off changes no canonical outcome.
- `cargo fmt -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` gate any implementation.
- Tests never touch external network (mock stub + pre-recorded fixtures).
- Plan-authority: Part I + WP + tests + ledger in one review.

## Interfaces (frozen; implementers use exactly these names)

```rust
// crates/interchange/src/hindsight/config.rs (new)
pub struct HindsightConfig {
    pub enabled: bool,        // default false; all else ignored when false
    pub base_url: String,     // e.g. http://127.0.0.1:PORT; no default
    pub bank: String,
    pub default_tags: Vec<String>,
    pub timeout_ms: u64,      // default ~1500
    pub max_queue: usize,     // drop-newest bound
}
pub fn hindsight_config_from_env() -> HindsightConfig // LTMRS_HINDSIGHT_*

// crates/interchange/src/hindsight/{types,ids}.rs (new)
pub struct RetainDoc { document_id: String, bank: String, tags: Vec<String>, title: String, text: String, project: Option<String> }
pub struct RecallHit { document_id: String, score: f64, snippet: String }
pub fn document_id(collection: &str, id: &ltmrs_domain::id::EntityId) -> String;
pub fn tag_for_scope(scope: &ltmrs_service::projects::ResolvedScope) -> Vec<String>;
```

- `document_id = "ltmrs:{collection}:{entity_uuid}"`, collection ∈ `memory|guide|session`, uuid = canonical `EntityId` (never legacy display id). Upsert on every dual-write (idempotent). `memory_forget` sends tombstone `retain` (`archived:true`), never delete.
- Plumbing: `DaemonConfig::hindsight: Option<HindsightConfig>`; `Dispatcher::with_hindsight(handle)`; tests use `None`.
- Recall fusion: canonical items first in existing order; Hindsight hits appended under labeled `hindsight_suggestions`, deduped by mapped id, scores as-is, never into native ranking. Down/disabled → local-only + `hindsight: "unavailable"` text hint. Never errors the tool.

## Behavior

Dual-write fan-out (post-success only, bounded queue drop-newest, shutdown drains ≤1 flush): `memory_add`, `memory_update`, `memory_feedback`, `memory_merge`, `memory_relate`, `guide_create`, `guide_update`, `guide_distill`, `session_end`. Explicitly NOT: forgets-as-deletes, reads, stats/audit/library, backup/restore, session_start/attempt. Post-redaction text only (`confirm=true` forwarding needs owner call OQ-4).

Recall fan-out: `memory_read`, `semantic_search`, `guide_get` — only when enabled AND query carries scope. `reflect` wired, manual-only in B1.

## Test plan

Pure unit (id format, tag derivation, fusion) in `interchange`; mock-client tests (upsert body, timeout→`BridgeError::Unavailable`, queue-full drop, worker never delays commit via paused clock); daemon enabled-with-mock tests (canonical success + mock received upsert per fanned tool; mock-down still success, no receipt change); mock-recall tests (order unchanged, appended+deduped, `all=true` no cross-boundary leak).

## Plan-authority updates (one review)

Part I new `§11.5 Hindsight bridge` (authoritative-local, best-effort remote, privacy rule). Part II new short WP referencing WP-11 (S7 delivered; placement: new WP). Part III new `T-HINDSIGHT-*` cases. Deviation ledger entry (external-content ordering + `confirm=true` rule).

## Open questions (decided in spec unless noted)

OQ1 auth: localhost no-auth default, bearer optional field (confirm in Phase 0). OQ2 scoping: single bank + per-doc project/channel tags (confirm vs precedent). OQ3 config: env `LTMRS_HINDSIGHT_{ENABLED,URL,BANK,TAGS,TIMEOUT_MS}` + `--hindsight-url/--hindsight-bank` flags; daemon log names state at start. OQ4: always post-redaction; `confirm=true` verbatim only if bank is loopback — OWNER CALL. OQ5: reflect manual-only in B1. OQ6: new short WP. OQ7: async worker owned by daemon (existing tokio); interchange exposes message types + thin async client (no tokio in interchange prod closure).

## Spec self-review

- Placeholders: none — OQs decided except OQ1/OQ2/OQ4 confirmations, which are Phase 0 outputs (explicitly scoped research, not implementation gaps).
- Consistency: authoritative-local vs best-effort-remote holds throughout (dual-write never fails tools; recall never reorders). Privacy §12.1 respected (post-redaction default).
- Scope: interchange + daemon wiring + recall fusion; no domain/service/compat contract changes.
- Ambiguity: `reflect` manual surface undefined in B1 — deliberate (B2); handle exposes it, nothing auto-calls it.
