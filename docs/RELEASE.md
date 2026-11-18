# ltmrs — v0.2-beta candidate: release status

Single tracker for the v0.2-beta candidate. One line per ticker: status + evidence pointer.
Gate definitions live in `plans/01_design_and_concepts.md` §97 and `plans/03_quality_tests_benchmarks_conformance.md` §5–9; the plan-of-record is `plans/02_implementation_guide.md` §19.

## WP-13 — release closure

| Ticker | Status | Evidence / owner |
|---|---|---|
| Suite on baseline + continuously | done | 2026-11-18 on the bump commit: fmt clean, clippy `-D warnings` clean, full workspace suite 1312 passed / 0 failed / 44 ignored, `ltmrs -V` = 0.2.0-beta |
| Native-code audit | done | No dependency changes across the release delta; getrandom audit stands; 594-component SBOM, no GPL/AGPL/proprietary — `reports/release-01/INDEX.md` |
| Offline install | done | `unshare -Unr`: provision control fails loudly; serving with pre-provisioned E5 green, zero routes |
| Migration + rollback | done | Migration suites + legacy-refusal green; rollback executed live end-to-end (backup/restore/rollback with assertions, exit 0) |
| Matrices published | done | `reports/release-01/matrices.md` + conformance_matrix.json; 32/32 tool cells executed, 4 gaps closed; remaining not_run cells environmental |
| Deviation review | open | 17-entry ledger: 14 owner-approved; DEV-007 re-approval pending; DEV-016/017 proposed, owner review pending — `baseline/lemma-0.21.0/deviations.json` |
| Archive bundle | done | `reports/release-01/` (INDEX, SBOM, digests, rollback transcript) |
| Publish v0.2-beta | done | owner approved 2026-11-18: main fast-forwarded to 61e49b54af, annotated tag v0.2-beta pushed to origin; v0.1-alpha published 2026-09-29 (538c0b2) |

## WP-12 — qualification

| Ticker | Status | Evidence / owner |
|---|---|---|
| `host_live`/`cold` legs | done | executed 2026-10-21 — `reports/wp12-*` records (host-live, cold, canary, backup/restore, import, replay, search) |
| `daemon` leg | done | executed 2026-10-21 daemon-reconnect leg, same record set |
| `kill9` leg | done | executed 2026-10-21 kill9 leg, same record set |
| Long-run clock soak | open | 4-segment soak executed 2026-10-21: 2000 ops / 0 failures / 119 conflicts, RSS stable, no leaks — `reports/wp12-soak-01/summary-soak.json`; 24h clock soak open |
| 32/32 tool matrix | done | executed + 4 gaps closed (2026-11-13 matrix batch: 7 oracle flips → executed_partial, 6 conformance gates, 13 tests); remaining not_run cells environmental — plans/02 §19 |
| Contended write | done | executed — 2000 ops / 0 failures, `reports/wp12-contended-write/` (summary.json, contention.json) |
| Upstream-differential re-run | done | executed 2026-10-28 + salted comparison 2026-11-12 (verdict corrected 2026-11-12: put NOT COMPARABLE by dedup design; gate behavior measured, not throughput) — `reports/wp12-lemma-01/`, `reports/wp12-lemma-salted/` |
| Timing differential | done | executed 2026-10-28, 6/6 metrics |
| Load run | done | executed 2026-10-20 — 811 ops / 0 failures closed and open, telemetry — `reports/wp12-load-01/` |
| Soak run | done | 4 segments executed (see long-run clock row for the open 24h ticker) |
| Search-load run | done | executed 2026-10-20 — 39 ops / 0 failures — `reports/wp12-search-load/` |
| BEIR SciFact full run | done | box closed 2026-11-13: 5183/5183 documents, CPU 1246s + CUDA 123s, identical metrics 0.644/0.576/0.837/0.589; 709-slice re-run identical after ranking default 0.70 (owner decision 2026-11-13) |
| CUDA qualification | done | executed 2026-10-21 on RTX 3080: full 2000-put + 400-search CUDA parity, CPU-fallback parity, 4 driverless legs, release-profile 4/4; CUDA build pinned 12.4.4+cuda12.4 (cargo), nvcc 12.4, driver 577 |
| Windows certification | done | 2026-10-08: fmt/clippy/test clean, named-pipe daemon + T-SEC-01 security suite green, `windows_certification` ledger; compile gaps fixed on-host 2026-11-13 (windows-sys dep, NTFS `event_filename`, safetensors skip guards, CRLF offset); per-host skill activation not covered |
| Power-loss durability legs | done | Alpine 3.24 libvirt rig built + proven 2026-11-13: canary written+synced survived destroy/reboot; 50MB mid-write load → journal-orphan recovery, canary intact (rigtest1); trials pass: `reports/power-wp12-smoke10/`, `smoke11` (12 steps green), `stale1` (verdict.json); `power clean --stale` sweep + teardown hardening 2026-11-14 — `reports/power-clean-stale/` |

## Standing not_run (environmental)

- **Power-loss hardware qualification** — the libvirt rig models power loss (guest dirty pages discarded on `virsh destroy`; host caches intact), not hardware/filesystem-specific proof. Qualification on a controlled dedicated machine remains open (plans/03 §4.1 doctrine; R1 adds the supported NVIDIA/CUDA configuration).
- **Multi-host matrices** — Linux R0 primary; Windows host certified 2026-10-08/2026-11-13; Alpine 3.24 rig guest for power legs. Broader host matrix not_run.
- **Upstream manual quality waves** — full upstream manual corpus waves open; quality gate open.
- **24h long-run clock soak** — open (4-segment soak executed, see above).
- **BEIR beyond SciFact** — SciFact full run closed; BEIR full-suite quality gate open.

Updated 2026-11-18.
