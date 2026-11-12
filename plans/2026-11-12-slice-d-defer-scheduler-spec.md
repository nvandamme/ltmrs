# Slice D — Defer-Scheduler Prompt-Cache Mechanics (Recall Shaping Only): Spec

## Goal

Adapt defer/replay + ride-the-bust scheduling + tiered compartments to ltmrs **recall shaping** (frontend instructions / snapshot rendering). Explicitly NOT canonical writes, receipts, budgets, or dispatcher behavior.

## Non-goals (pinned)

Canonical writes, operation receipts / idempotency, conflict/contention budgets (`admission.rs`), scan-snapshot cache semantics, `injection_score`/ranking (D tiers select presentation fullness only, never reorder recall), embedding/search pipelines. Touch nothing in `crates/daemon/src/dispatcher.rs`, `crates/domain` command/receipt paths, `crates/service` admission/budgets, `export_snapshot`/scan-cache.

## Architecture

All logic lives in `crates/frontend` (instruction/snapshot rendering) + pure helpers in `crates/compat` or `crates/frontend/src/frontend/`. Touch nothing listed above.

Spec lives in `plans/` (tracked).

## Global constraints

- Presentation-only: compartment assignment never mutates history rows; summaries (if any) marked derived, never written back.
- RV-17: shaping must not persist read side effects (counters/confidence untouched).
- No I/O, no clock in pure helpers (`now_millis` injected like `injection_score`).
- `cargo fmt -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` gate any implementation.
- Budget is a *render* budget (bytes/items for prompt) — name it `render_budget` everywhere; distinct from service budgets and scan row-budget.
- Plan-authority: Part I + WP + tests + ledger in one review.

## Interfaces (frozen; implementers use exactly these names)

```rust
// crates/frontend/src/frontend/defer.rs (new, pure)
pub fn render_snapshot(view: &SnapshotView) -> String;
pub fn defer_key(prefix: &[u8]) -> DeferHandle;
pub fn replay(handle: &DeferHandle, suffix: &str) -> String;

// crates/frontend/src/frontend/tier.rs (new, pure)
pub enum Tier { Full, Summary, Pointer, Drop }
pub fn tier(age_ms: u64, importance: f64, budget_bytes: u64) -> Tier;

// crates/frontend/src/frontend/compartments.rs (new)
pub enum Compartment { Pinned, RecentFull, Summarized, PointerOnly }
pub fn assign(history: &[HistoryRow], budget: u64, now_ms: u64) -> Vec<(Compartment, HistoryRow)>;
```

- Deterministic renderer (fixed key order, fixed float formatting, `\n` endings). `replay` asserts byte-prefix identity (`prefix == &out[..prefix.len()]`); renderer change invalidates outstanding defers (version-tag; mismatch → re-render, never patch).
- `tier()` monotone: older/larger-over-budget ⇒ never upgrades tier; formula + constants documented.
- Ride-the-bust: concurrent identical renders coalesce onto one in-flight render (single-flight); late joiners get the same bytes.

## D1 memo (2026-11-12, no code changed)

Shaping seam: `build_instructions_index` (`crates/frontend/src/frontend/mcp.rs:136`) — sorts by confidence, takes top-8 project + top-5 global, renders fixed `"  [{id}] {title} ({conf:.2})"` lines into the MCP `instructions` field. This is the single insertion point for D2 defer/replay + D3 tier + D4 compartments.
Secondary seam (unchanged, noted only): `shape_result` (`crates/frontend/src/frontend/mcp/handler.rs:140`) shapes per-call `ToolResult` text — out of scope for D (per-call text is not snapshot rendering).
Forbidden files (D touches none): `crates/daemon/src/dispatcher.rs`, `crates/domain/**` command/receipt, `crates/service/**` admission/budgets, `export_snapshot`/scan-cache, `injection_score`/ranking, embedding/search pipelines.
`importance` default confirmed per spec OQ-D1: confidence + access-count combination, separately named (`recall_importance`, not `injection_score`). `render_budget` default confirmed per spec OQ-D3: per-call param defaulting to static config (8 top-project + 5 global rows = current shape; budget in rows first, bytes later).

## Test plan

Pure unit + property tests in `crates/frontend` (no daemon, no I/O): byte-identity, single-flight coalescing, tier boundaries, budget cap, store-unchanged-after-render.

## Plan-authority updates (one review)

Part I recall-shaping boundary section + tier-function spec (formula + constants table). Part II renderer determinism rules + compartment definitions + budget accounting. Part III conformance cases (byte-identity, budget, non-mutation). `traceability.json` + `conformance_matrix.json` entries; enhancement/deviation ledger note (RQ-15: shaping is presentation, not behavior deviation).

## Open questions (decided in spec unless noted)

OQ-D1 `importance`: `injection_score`-adjacent but separately named (avoid coupling); default: confidence + access-count combination — OWNER CONFIRMS formula in Phase D3. OQ-D2 summaries: truncation/pointer-only first (no generated summaries in v1). OQ-D3 `render_budget` source: per-call param defaulting to static config — OWNER CONFIRMS in Phase D1 memo.

## Spec self-review

- Placeholders: none — OQs carry defaults; owner confirmations are review gates, not gaps.
- Consistency: presentation-only holds (no canonical mutation; store-hash test pins it); `render_budget` naming prevents budget confusion.
- Scope: frontend-only; forbidden-files list enforced in Phase D1 memo.
- Ambiguity: `importance` formula has a concrete default; tier monotonicity is testable regardless.
