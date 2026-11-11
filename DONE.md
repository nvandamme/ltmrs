# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`
- Slice B silent-capture hooks (opt-in passive recall/capture, explicit-tools contract unchanged) — all 5 phases, TDD RED-first:
- Phase 1: `hook-inject`/`hook-extract`/`install-hooks` CLI subcommands (mutually exclusive positional words); `help_text` documents all three; `skills/hooks.rs` gate (missing/unparseable config => disabled); main dispatch arms (Unimplemented until Phases 2-4 landed). Tests: `hook_subcommands_parse`, `help_documents_hooks`.
- Phase 2: inject attaches to the RUNNING daemon over its socket (never spawns — singleton lock); handshake + frontend share ephemeral ids (fixes `frontend_mismatch` found RED); lexical `semantic_search` top-5, 5s timeout, 4k-char cap, fail-open. Test: `hook_inject_returns_matching_recall_live` (live seeded daemon).
- Phase 3: extract tails transcript to 8KB, splits DECISION/LESSON/FACT/NOTE candidates (<=5), secret pre-filter, one `memory_add` each (`source: hook-extract`, server Jaccard dedups reruns). Tests: store + rerun-idempotent + secret-excluded.
- Phase 4: `install_hooks` atomic write with version marker (`Installed`/`AlreadyCurrent`/`RefusedForeign`); `hook_snippet("claude-code")` template; `install_hooks_command` + main wiring + stdin-JSON helpers; skill doc opt-in paragraph (workflow list byte-identical, pins green). Tests: installer + snippet.
- Phase 5: plan-authority one-review — Part I §11.3 hooks principle, Part II WP-10 task, Part III T-HOOK-01, RQ-25 evidence+test, spec at `plans/2026-11-12-slice-b-silent-capture-hooks-spec.md` (tracked; `.kilo/` rejected per user order).
- Proof: frontend lib 137/137, daemon lib 355/355; `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -D warnings` clean.
