# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`

---
## Unreleased Commit
- Slice C Magic Context bridge (read-only import, TDD RED-first):
- C1 reader + version fence: excluded `crates/mc-import` (own workspace, cdylib, own rusqlite edge); `open_readonly_copy` (tempfile copy, magic+WAL check, query_only, source-unchanged); `MAX(version)` fence v95 (matches upstream `LATEST_SUPPORTED_VERSION`); fixture builder. Tests: supported/newer-refuses/WAL-refuses/unchanged (4/4).
- C2 scope mapping: `scope.rs` reads `session_projects`; closed harness vocabulary (opencode/opencode2/pi/omp/claude-code); absolute-path + `..` + non-repo + symlink-escape refusals; git-root canonicalization; same-path-different-roots differ. Tests: 4/4 (11/11 mc-import total with C1+C3).
- C3 row mapping: `map.rs` McMemory->Memory (external_alias `mc:{id}`, UUIDv5 deterministic ids, tags `mc-category:*`, loss for importance/scope/seen/retrieval; non-active refuses loudly; embeddings opaque).
- C4 CLI + atomic publish: `skills/mc_import.rs` dlopen loader (candidate paths + env override, mirrors lemma_import); preview (staging JSON -> counts/digest/loss/scopes); `confirm`/`confirm_staged` (digest-bound, memory_add fan-out, rerun-safe); `mc-import preview|confirm` CLI verbs + dispatch + help; hermetic tests (pure parse, candidate error, live confirm 2-applied + rerun-0).
- C5 plan-authority one-review: Part I §11.6, WP-10 task, T-MCIMPORT-01, RQ-16 evidence+test; no daemon/compat change (CLI-first per OQ-C4).
- Proof: mc-import 11/11, frontend lib 164/164, daemon lib 355/355; fmt clean; workspace clippy -D warnings clean.
