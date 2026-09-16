# CHANGELOG.md

Work completed on previous commits, grouped by commit.
Content before `---` is instructions — do not modify. Add entries after the `---`.

---

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

## dd0be90 (2026-09-16) — close WP-00 gaps from plan review

- Added static tool-definition snapshot (`static/tools_static.json`), kept
  separate from the live `tools/list` (dynamic description injection).
- Recorded DB schema version (8) and static/live snapshot refs in
  `upstream-lock.json`.
- Added behavior classification (schema-documented / handler-defined /
  side-effect / apparent-defect) to the behavior inventory.
- Documented `tools/list_changed` notification; captured real error
  responses (unknown tool, bad argument) for the T-MCP-03 baseline.
- Aligned `tokenizers` to 0.22.2 (candle-core's version) to remove a
  duplicate from the dependency graph (593 -> 590 packages).
- Extended the native-code audit (F-05, model-license deferral).

## e75b963 (2026-09-16) — WP-00 baseline capture and dependency lock (S0)

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

## 9614885 (2026-09-16) — initial repository with implementation plans

- Added the reviewed implementation specification and test plan (`plans/`):
  design & concepts, implementation guide (WP-00…WP-13), quality/tests/benchmarks/conformance,
  plus `traceability.json` and `conformance_matrix.json`.
- Added `README.md` (overview, status, architecture, constraints) and `AGENTS.md`
  (agent working rules, plan authority, evidence discipline).
- Added dual license (`LICENSE-MIT`, `LICENSE-APACHE`), `rust-toolchain.toml` (1.96.0 baseline),
  Cargo skeleton, `.gitignore`, and IDE/MCP configs.
