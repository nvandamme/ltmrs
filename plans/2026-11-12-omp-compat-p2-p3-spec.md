# omp Backend Compat: P2 (memory_edit) + P3 (status surfaces) Spec

## Goal

Make ltmrs wire-compatible with omp's agent memory tools that have no direct equivalent: `memory_edit{op,id,content?,importance?,replacement_id?}` and the backend status surfaces (`status/stats/diagnose/queuePreview`, `/memory` slash commands). P1 (redaction hardening) already landed.

## Non-goals

No canonical behavior change. No new storage. No ranking change (RQ-11). No 24-tool mnemopi verbatim port. No omp-side adapter (different repo).

## P2: `memory_edit` tri-op alias (native tool)

omp `memory_edit` (mnemopi-only): `{op: update|forget|invalidate, id, content?, importance?, replacement_id?}` with a read-only-fact guard (`not_editable`). ltmrs mapping:
- `update` + content → `memory_update` (fragment/title patch).
- `update` + importance only → `memory_feedback`-style confidence adjust? NO — importance is display-rank in mnemopi; ltmrs confidence is earned. Map importance-only updates to a no-op success with a note (honest, not silent coercion), OR reject. DECISION: reject importance-only updates with a clear error (`importance is earned in ltmrs, not set`), per RV-16 (no coercion).
- `forget` → `memory_forget` (same id).
- `invalidate` + replacement_id → `memory_merge` consolidate path? ltmrs has no invalidate state... Actually `MemoryLifecycle` has `Invalidated`/`Archived` variants. Check how memory_forget maps modes first; `invalidate` likely maps to a forget mode. DECISION in implementation: reuse forget modes, document mapping.
- Read-only-fact guard: ltmrs has no read-only facts; every stored fragment is editable by its scope. Return success (no guard needed), document the deviation.

Surface: 6th native tool `memory_edit` (after backup_* + ... wait, Slice B added no tools; native tail is backup_create/preview/restore/import_lemma = 4). `memory_edit` becomes 5th native. Update: `served_tools_match_frozen_schema` (26+5), `served_tools` count 30→31, conformance matrix `expected_tool_count`, traceability RQ-15, Part I §11 (native list), Part II WP-10 task, Part III T-MEMOEDIT-01 (or fold into T-HOOK... no — new T ID).

Fail-open? No — it's an explicit tool, errors loudly like all tools.

## P3: status/stats/diagnose markdown surfaces

omp backends render `status/stats/diagnose/queuePreview` markdown for `/memory` slash commands. ltmrs has the data (memory_stats, memory_audit, sessions, restore coordinator). Implementation: pure render functions in `crates/frontend` (or daemon? — read-only, no IPC needed if computed from existing tool outputs... but slash commands run host-side).

DECISION: implement as three pure formatters over existing `DomainPayload`s + a `queue` view over the bridge outbox depth (when configured) — host calls existing tools, formats locally. No new IPC. Files: `crates/frontend/src/skills/memory_status.rs` (new): `render_status(stats_payload, audit_summary) -> String`, `render_diagnose(...)`, `render_queue(...)`. Tests: golden pins on fixture payloads (honestly labeled, not upstream oracles).

Plan-authority: Part I §11.3 (status surfaces), Part II WP-10 task, Part III T-HOST-01 extension (or T-STATUS-01), RQ-25 evidence.

## Test plan

P2: RED — memory_edit update maps to memory_update (content round-trips); forget maps; invalidate maps to documented mode; importance-only rejects loudly; unknown id errors (not silent); unknown op rejects. P3: golden pins on rendered markdown (fixture payloads, byte-stable).

## Open questions

OQ1: exact `invalidate` → forget-mode mapping (check `ForgetMode` variants first). OQ2: does `memory_edit` need `confirm` semantics for forget? (match memory_forget's existing confirm behavior — yes, reuse). OQ3: should P2 live behind a flag? No — native tools are always served (backup_* precedent).

## Spec self-review

- Placeholders: none — OQ1/OQ2 have defaults (check variants, reuse confirm).
- Consistency: no canonical change; alias + render only.
- Scope: one compat module + one render module + tests + authority.
- Ambiguity: importance-only rejection is explicit, not silent.
