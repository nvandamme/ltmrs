# TODO.md

Planned work, implementation plans, and backlog notes.
Content before `---` is instructions — do not modify. Add entries after the `---`.

---

## Active — S0 (Reproducible baseline)

- [x] WP-00 — Freeze sources, dependencies and compatibility inputs
- [ ] WP-01 — Domain types and executable reference model

## Next — S1 (Atomic memory)

- [ ] WP-02 — Backend capability and correctness gate (Lance-only, then Fjall+Lance)
- [ ] WP-03 — Harden the canonical repository

## Backlog (later slices)

- [ ] WP-04 — Singleton daemon, IPC and session routing (S2)
- [ ] WP-05 — Versioned Lance search projection (S3)
- [ ] WP-06 — Candle embedding service and model qualification (S4)
- [ ] WP-07 — Retrieval, graph context and explanations (S5)
- [ ] WP-08 — Complete memory MCP contract (S2/S6)
- [ ] WP-09 — Guides, sessions and intelligence (S6)
- [ ] WP-10 — CLI, managed skills, hosts and visualizer (S6)
- [ ] WP-11 — Import, legacy interchange, backup and restore (S7)
- [ ] WP-12 — Qualification, quality and performance (S8)
- [ ] WP-13 — Release, documentation and evidence closure (S8)

## Open decisions (from Part I §13)

- [ ] AD-01 — Canonical backend A (LanceDB) or B (Fjall) — closes at WP-02
- [ ] AD-02 — Exact crate/feature/toolchain lock — input from WP-00 audit
- [ ] AD-03 — Legacy contract baseline wire capture — done in WP-00
- [ ] AD-04 — Default Candle recipe (E5-small candidate)
- [ ] AD-05 — Score/abstention thresholds
- [ ] AD-06 — FTS freshness / null-vector behavior for pinned Lance
- [ ] AD-07 — Host/platform support matrix
