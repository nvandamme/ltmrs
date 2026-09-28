# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`.

---

## Unreleased Commit

### Provenance vocabulary (DEV-004, owner-agreed) (2026-09-28, uncommitted)

- `MemorySource` expanded beyond user|ai: user|ai|web|paper|book|code
  with disjoint documented rules (URL-captured vs DOI/arXiv-identified
  vs ISBN-identified vs path-identified; verification state stays out
  — feedback counts already model it). Ingress (tools.rs parse+coerce),
  stats (dynamic by_source), icons (non-ai → person, upstream parity),
  serde round-trip all covered; truly-unknown still coerces to ai
  (tested residual, e.g. 'user-corrected formal review').
- Tests: provenance_vocabulary_round_trips (RED-first) +
  memory_stats_groups_expanded_provenance. Ledger narrowed
  (behavior, impact, test, release_wording); owner-approved.

### Deviation approvals + 008/009 follow-ups (2026-09-28, uncommitted)

- Approvals flipped to `approved by owner 2026-09-28` for all 12
  (001/002/003/005/006/007/010/011/012 first, then 004/008/009).
- DEV-008 answer: baseline captures mechanisms only (seed list in
  prompt; 4 seeds; coaching blocks; tech autodetect; distill
  suggestion) — no content/heuristics. Filed as TODO backlog
  item (not a plan WP: host/model concern, not canonical contract).
- DEV-009 answer: mechanism verified NOW (smoke + live probes to
  the process boundary); per-host uptake is FUTURE (needs real
  hosts; unobservable from inside). Documentation approved as
  accurate.
- Validation: fmt/clippy clean, 657 lib + 2 smoke green.
- Approvals: all 12 deviations now `approved by owner 2026-09-28`.

### Deviation fix investigation + release dual review (2026-09-28, uncommitted)

- All 12 deviations investigated for FIXES (not just wording):
  intentional-and-kept: 001/005 (core retrieval), 002 (hardening),
  003 (correctness; keep-open preserves binding), 006
  (architecturally impossible), 007 (determinism preferred; 0.80
  verified at tools.rs:1084,1349), 008 (architecture; no-seed
  verified), 010 (refusal IS RQ-21 compliance), 011/012 (core
  features, now production-true).
- DEV-004 built (see Provenance entry above): expanded corpus
  implemented + tested; ledger narrowed.
- DEV-009 partially verifiable only: mcp-stdio to the process
  boundary is proven (smoke + live probes); host-side uptake is
  unobservable from inside. Ledger already says exactly this.
- Wording fixed in ledger (2 lines, JSON valid): `calibrated`
  → normalized + AD-05-open in DEV-001/005. Approvals complete
  (owner; see Approvals entry).
- Release dual review closed: socket-mode E5 unwired (no config
  producer — stdio-only dense, documented); serve() wiring is
  consistent future-proofing; backup excludes weights by
  construction; restore re-enqueues jobs; conformance matrix has
  no retrieval rows; absolute-claims sweep clean; Cargo/native
  policy intact (no new deps); version bump safe (skill versions
  independent, suite green).

### WP-13 release tickers: suites, offline, rollback, matrices, ledger, archive, version (2026-09-28, uncommitted)

- Suites: `cargo test --workspace --all-targets` 655 lib + 2 smoke,
  0 failed (4 ignored); fmt/clippy clean. Compat fixtures run
  in-suite; upstream-differential bench not re-run (no contract
  change; upstream server not provisioned — honest not_run).
- Offline (ENFORCED via `unshare -Unr`, not just absence-of-use):
  provision-in-netns control fails loudly exit 1; full serving flow
  (add/search/read, E5 provisioned) exit 0 with zero routes.
  Reqwest audit: only the provision CLI builds network clients.
- Migration: unit suites green; legacy refusal tested; rollback
  instructions now exist (README Operations) and were executed
  live (see Restore entry above for the full cycle; blockers
  found there are fixed, not waived).
- Matrices: `reports/release-01/matrices.md` (tool 117/115 cells
  referenced, host, model, OS, durability — executed or not_run).
- Deviations: all 12 reviewed claim-by-claim (0.80 threshold,
  passthrough, no-seed verified); fixed `calibrated` → normalized
  (AD-05 open) in DEV-001/005; all approved (see Approvals entry).
- Archive: `reports/release-01/` (INDEX, sbom 594 comp — no
  GPL/AGPL/proprietary; binary/lock/manifest/fixture digests;
  rollback transcript; suite record). Version: `0.1.0-alpha`
  (§2.3; skill versions independent, suite green, binary reports
  it). Publish (tag/push) needs owner approval — NOT done.

### Operator docs: E5 + Operations README sections (2026-09-28, uncommitted)

- Codebase scan for implemented-but-undocumented operator surface
  found: E5/provision behavior, managed-home layout, full CLI
  command map, exit codes, visualizer token auth, backup/restore
  shape, no-config-file rule. None were in README (limits lived
  only in CHANGELOG history).
- Added `## Dense retrieval (E5)` (provision, enablement,
  async-indexing contract, costs, edge semantics) and
  `## Operations` (layout table, command map with `--help`
  authoritative, exit codes, token URL/port/403, backup tool
  chain). Every sentence verified against mechanism, test, or
  measured run before writing.
- Left uncommitted (no commit ordered). Health/doctor report is
  intentionally NOT documented: `health_report` has no CLI or MCP
  trigger (dead-ish API) — documenting it would be a false claim.

