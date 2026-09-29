# Release tickers — v0.1-alpha candidate

Living tracker for the release checklist (WP-12 qualification,
WP-13 closure). Statuses: `done` (executed, pointer), `open`
(explicit reason + owner). Updated 2026-09-28. Normative spec stays
in `plans/`; history in `CHANGELOG.md`; evidence bundle in
`reports/release-01/` (gitignored).

## WP-13 — release closure

| Ticker | Status | Evidence / owner |
|---|---|---|
| Full suites (fmt/lint/unit/integration/recovery/conformance) | done | 664 lib + 2 smoke green (7 ignored); compat fixtures in-suite |
| Native-code policy | done | no dep changes; 594-comp SBOM, no GPL/AGPL/proprietary |
| Offline install (fresh HOME, pre-provisioned, no network) | done | enforced `unshare -Unr` proof (control fails, serving exit 0) |
| Migration + rollback instructions | done | unit suites + live rollback cycle transcript |
| Matrices (tool/host/model/OS/durability) | done | `reports/release-01/matrices.md` |
| Enhancement/deviation review | done | 12/12 reviewed, wording fixed, all owner-approved |
| Archive bundle | done | `reports/release-01/` (INDEX, SBOM, digests, transcript) |
| Publish v0.1-alpha | open | tag/push — **owner approval required** |

## WP-12 — qualification

| Ticker | Status | Evidence / owner |
|---|---|---|
| Deterministic generator + recorder | done | in-suite determinism tests |
| Storage-only tests | done | wp12-storage-01 (2000 ops, 0 failures) |
| Closed/open-loop load tests | done | wp12-load-01 + fresh 2026-09-28 re-run on release tree (811 ops, 0 failures): closed gets p50 3µs/p99 46µs, puts p50 16.3ms/p99 60.6ms; open Poisson-200/s puts p50 10.6ms/p99 11.3ms, tight tails |
| Recorded run metadata | done | histories + store bytes + durability mode |
| Fault tests (point injection + fragmented kill) | done | migration-atomicity, unknown-outcome, barrier faults; fragmented kill; kill/reopen suites |
| Soak + sustained-load | done | deterministic soak (1000 mixed ops, last-write-wins verified) + sustained racing-write consistency; in 660-suite |
| Long-run clock-time soak + fault under sustained load | open | **no harness exists** (new work package, not a review fix) |
| Labels (10-case safety fixture) | done | schema + validator in-suite |
| 300-case corpus (synthetic known-answer) | done | `experiments/quality/retrieval-calibration.json` (300/300, topic-disjoint dev/heldout, paraphrase queries); human-reviewed corpus still open |
| Ablations (lexical/deterministic/E5 legs) | done | executed legs green |
| Remaining ablation legs | open | no implementation behind them (honest not_run) |
| No-answer/quality metrics | done | pure metrics + zero-tolerance asserts on executed legs |
| Calibration on dev split | done | 0.70 candidate (full dev+heldout retention, thin margin); default held at 0.0 — absolute E5 similarities saturate high, margin too thin on synthetic data to move it |
| Agent wave (LLM-judge retrieval) | done | 20 heldout cases, agreement 0.389 → 0.632 few-shot; exact-target hit@1 15/20; `agent-wave.json` in bundle (evidence with caveats, not a gate) |
| Raw results published | done | JSONL histories + summaries + op export + driver |
| Storage re-run on release tree | done | 1614 ops 0 failures (putget) + 386 ops 0 failures (search); summaries in bundle |
| Upstream 100-op sample | done | fresh transcript in bundle (26 ok / 74 read-miss-expected); full differential unchanged (no contract delta) |
| BEIR SciFact harness | done | ignored test + UKP corpus (5183 docs / 300 queries); proven through 709 indexed docs |
| BEIR SciFact full run | open | **~5h CPU bulk indexing** (measured 4s/job: multi-chunk forwards + per-job commits); needs GPU or unattended box |

## Standing not_run (environmental)

| Item | Reason |
|---|---|
| Upstream-differential re-run | bounded 100-op sample fresh (see above); full run tied to WP-12 evidence (no contract delta to re-verify) |
| Timing differential, salted 100-op stream (dedup defeated both sides) | done | put med 4.4→1.2ms, search med 1.2→0.2ms (ltmrs faster; small stores, lexical paths, same box); transcripts in bundle |
| Power-loss qualification | needs a controlled dedicated machine |
| Upstream manual quality waves (c2/wave2/wave3/glm-agent) | need live LLM agents driving MCP sessions; RRF/MMR math parity verified in-suite instead |
| Multi-host matrices | single host executed; no other hosts available |
