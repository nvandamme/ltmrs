# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`.

---
## Unreleased Commit

- Double review vs Lemma target (arch + functional, ADRs kept) + plan/ADR/trackers recheck — 3 code gaps fixed, RED-first:
- FuncReview C1 (critical): `exec_memory_merge` wrote `project_id: None` while `memory_add` dual-writes the stable id — merged rows globalized out of their scope. Fix threads `resolved.project_id` + name/legacy fallback (Wave A3b parity); RED test `merge_keeps_stable_identity_on_result` (pre-fix: merged row `None`, expected `Some(id)`).
- FuncReview M1 (major): `is_mutating_tool` was a 7-name parallel string list that drifted from `mutates_store()` (backup_import_lemma + all guide/session/suggestion mutations never notified). Fix: single `ToolName::notifies_mutation()` predicate in compat, handler delegates; notify parity test covers all 30 served tools (round-trip + gate==predicate per tool + mutating/silent sets).
- FuncReview N1 (minor): scan budget counted only memories+relations while guides/sessions rode free via a duplicated `QuotaTracker::check_scan_budget` that nothing called. Fix: single enforcement at `Dispatcher::scan_snapshot` over `CanonicalExport::row_count()` (all 9 collections); removed `ScanBudgetExceeded` + orphan gate test (covered by `scan_snapshot_honors_configured_budget`).
- Arch stray: deleted untracked-parity `crates/search/src/similarity.rsx` (stale monolith-era twin of `similarity.rs`, unreferenced; `mod similarity` resolves to `.rs`).
- Remnant sweep: zero stale refs — deleted `scan_budget_gate_trips_over_limit` renamed in both `conformance_matrix.json` and `traceability.json` RQ-22 evidence; `check_scan_budget`/`ScanBudgetExceeded`/`similarity.rsx` zero hits repo-wide; orphan-name sweep clean; gate-audit 13/13 PASS post-fix.
- Plan one-review repairs (§10.2a test names, WP-04 budget ref, §13/§20/WP-12/13 boxes, bulk-drive/Windows/split drift, AD-01 versions) — plan authority rule: requirement change = Part I + WP + test + ledger in one review.
- Proof: daemon lib 355/355, frontend lib 130/130, compat 46/46, domain 56/56; `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -D warnings` clean; gate-audit 13/13 PASS.
