# AGENTS.md

Project instructions for coding agents working in this repository.

## Repository

- Repository: `ltmrs` (Long Term Memory RS) — local MCP memory service for LLM agents.
- Language: Rust (edition 2024), single Cargo package with library + binary.
- Default branch: `master`.
- Toolchain: pinned in `rust-toolchain.toml` (stable, see AD-02 in the plans).
- Plans in `plans/` are the normative specification. Read them before implementing.

## Working Rules

- Keep changes focused on the user's current task; do not revert unrelated user changes.
- Prefer existing project patterns over new abstractions.
- Keep changes minimal and elegant: no unneeded refactoring, import reordering or restyling.
- **Plan authority (STRICT)**: `plans/01_design_and_concepts.md` defines contracts,
  `plans/02_implementation_guide.md` defines work packages, `plans/03_quality_tests_benchmarks_conformance.md`
  defines evidence. Changing a requirement requires updating Part I, its work package,
  the matching test and the conformance/deviation ledger in one review.
- **Evidence discipline (STRICT)**: never conflate planned/inspected/executed/passed.
  Unrun tests carry no invented values. A README or plan statement is not a passing gate.
- **No shortcuts on the hard parts**: atomic canonical commands, revision/uniqueness
  enforcement, session isolation, durability barriers and safe restore are non-negotiable.
  A recoverable partial merge is not an atomic merge; a fast buffered write is not a
  durable write. Choose the complex correct path over a fragile workaround.
- Do not add copyright or license headers in source files unless explicitly requested.
- Do not commit secrets, model weights, private memories or raw result dumps; reference
  them by digest/provenance.

## Rust Rules

- Write idiomatic modern Rust (edition 2024, current stable).
- `cargo clippy -D warnings` and `cargo fmt` must pass on all touched code.
- Prefer strict types: no `any`/`unknown` equivalents; define concrete types,
  newtypes for IDs and validated domain types. `unsafe` requires a documented
  safety argument and a test.
- Keep database crates and Arrow types behind `storage/` and `search/` modules.
  Expose domain types to the rest of ltmrs.
- No comments unless they explain non-obvious invariants, contracts or protocol
  details. No backward-compatibility shims.

## Validation

Run after code or test edits:

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release   # for release-claim work
```

Include the exact validation commands used in the report or `DONE.md`.

## Code Search — Always Use CCC (MCP Tools First, Then Skill)

- **Rule:** For all code search, semantic search, LOC boundary extraction and
  understanding unfamiliar code, use the CCC (cocoindex-code) tools instead of raw grep/glob/find.
- **Priority order:**
  1. **CCC MCP tools** (`cocoindex-code_search`): primary tool for semantic/code discovery.
  2. **CCC skill**: load the `ccc` skill for management or configuration tasks.
  3. **Raw grep/glob/find**: last resort for exact text or filename matching only.

CLI tools: `ccc init`, `ccc index`, `ccc search`. The index lives in `.cocoindex_code/` (gitignored).

## Installed MCP Servers

- **RustRover / JetBrains**: use IDE-integrated tools (build, symbol search, call
  hierarchy, diagnostics) over external CLI equivalents when available.
- **ccc**: cocoindex-code semantic search (see above).
- **lemma**: persistent cross-session memory. Call `memory_read` at task start,
  `memory_add` / `session_attempt` / `session_end` at task end.

## Git Rules

- Conventional commit tags on the first line (`feat:`, `fix:`, `refactor:`, `chore:`,
  `docs:`) followed by a short summary; full changelog in the body.
- ALWAYS use `--no-pager` with git commands (e.g., `git --no-pager diff`, `git --no-pager log`).
- Never amend or rewrite published history; never force-push (unless the user explicitly
  authorizes a specific history rewrite).
- Do not commit unless explicitly asked.
- **NO COMMIT SPREE (STRICT)**: one reviewed, coherent commit per logical unit of work.
  Review the change against the plan/spec BEFORE committing — never commit first and
  patch in a follow-up commit. If a review uncovers gaps, fold them into the same
  commit (amend while unpublished) instead of stacking fix-ups.

## Tracking Rules

- On all markdown tracker files, content before `---` is instructions and should not
  be modified. Only add content after the `---` line. Do not introduce new `---`
  separators, sections, or headings unless explicitly instructed.
- Use `TODO.md` for planned work, implementation plans, and backlog notes.
- Use `DONE.md` for work completed on the current unreleased commit (after `## Unreleased Commit`).
- Use `CHANGELOG.md` for work completed on previous commits, grouped by commit.

## Temporary Files and Scratch Space

- Use project-local `./tmp/` (gitignored) for scratch probes, quick tests, throwaway
  scripts and debug outputs instead of system `/tmp`.
- Reserve `experiments/` (tracked) for experiment work worth preserving in git history;
  keep bulky generated artifacts there out of version control via an in-directory `.gitignore`.
- Generated reports go to `reports/` (gitignored); release evidence is archived separately.
