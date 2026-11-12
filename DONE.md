# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`
- Slice A Hindsight bridge B1 (opt-in live dual-write + scoped recall, TDD RED-first):
- A0 precedent freeze (no code): omp client.ts wire table frozen into `interchange::hindsight::precedent` (retain/recall/reflect paths, snake_case fields, timeouts, Bearer auth, localhost:8888 default).
- A1 types + config: `RetainDoc`/`RecallHit`/`document_id` (ltmrs:{collection}:{uuid})/`tags_for_scope`/`hindsight_config_from_env` (LTMRS_HINDSIGHT_*); 6 tests.
- A2 client + worker: mockable `HttpTransport` (native async traits, no new dep; reqwest json feature + tokio dev-only), `HindsightClient` (retain/recall/reflect), `BridgeHandle` bounded drop-newest worker; 4 mock tests. Real finding: repo-wide env-var test race -> serializing Mutex guard.
- A3 daemon wiring: `Dispatcher.hindsight` (Mutex for late attach) + `with_hindsight`/`set_hindsight`/`enqueue_bridge_retain`/`tags_for_retain`; execute_tool post-commit fan-out; `DaemonConfig.hindsight` + `Daemon::start` build + `LTMRS_HINDSIGHT_*` env plumbing in both serve paths. Real findings: stale-parallel-list drift avoided via single `retain_doc_for`; `frontend_mismatch` avoided by unifying handshake/envelope ids (B pattern reused).
- A4 recall + fusion: `merge_hindsight_hits` (content dedup vs canonical text, repeat dedup, labeled section, never into ranking); `BridgeHandle::recall_section` (block_in_place+timeout, local-only without runtime); execute_tool hook for memory_read/semantic_search/guide_get. Tests: fusion 3 + live recall (canonical first, section appended) green.
- A5 plan-authority one-review: Part I §11.7, WP-10 task, T-HINDSIGHT-01, RQ-15 evidence+test; gate caught + fixed a real tracker regression (C edit had dropped T-SHAPE-01/T-HOST-01 rows — restored, 13/13 PASS).
- Proof: interchange 47/47, daemon 359/359, frontend 164/164; fmt clean; workspace clippy clean; gate-audit 13/13.
