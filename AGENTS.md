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

### Module Organization and Code Hygiene (STRICT — applies repo-wide)

1. **Domain modules own their features** — All code belonging to a domain lives in that domain's module, not scattered at the package/experiment root. A domain is a cohesive set of related features (e.g. graph logic, retrieval, probing, export). Code for a domain MUST live in a module named after that domain, never as loose files at the experiment level. *Example: in the cross-view experiment, graph logic lives in `geo_graph/` (e.g. `geo_graph/observation.py`, `geo_graph/policy/...`), never as loose `*_graph.py` files at the experiment root.*
2. **Split by intent/domain, not by size alone** — When a file grows beyond ~600–800 lines or mixes distinct concerns, split it per intent/domain. When a domain has multiple related modules, make it a subpackage. *Example: `geo_graph/policy/` is a subpackage because it has ≥2 related modules (assembly, builder, contract, dto, etc.).*
3. **No code duplication** — Never duplicate logic across modules. Maintain shared functions/features in a single owning module and import them. If a helper is used by ≥2 modules, it belongs in a shared/common module for that domain (or a cross-module utility if truly generic). Before writing a helper, search for an existing one.
4. **These rules are permanent** — They apply to the whole repository and all future work. Do not violate them for convenience; restructure existing code to comply when you touch it.

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
- **Deterministic async tests (STRICT)**: for time-based behavior (intervals,
  schedules, timeouts, backoff) use tokio's paused clock via
  `#[tokio::test(start_paused = true)]` — never real-time sleeps that race the
  scheduler. The `test-util` feature is authorized as a dev-dependency specifically
  for this; add it if scheduling/time tests need it rather than working around it.

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
- **NO COMMIT SPREE (STRICT)**: one reviewed (see commit gate rule below), coherent commit per logical unit of work.
  Review the change against the plan/spec BEFORE committing — never commit first and
  patch in a follow-up commit. If a review uncovers gaps, fold them into the same
  commit (amend while unpublished) instead of stacking fix-ups.
- **COMMIT GATE (STRICT)**: no commit until at least two full review-and-fix passes are
  done — `impl + 2 x (review + fix) === 'COMMIT ALLOWED'`. The two passes must be distinct:
  1. Formal review — re-read the code against plans/ requirements and verify coverage.
  2. Functional review — hunt for bugs/logic flaws (off-by-one, races, edge cases).
  Each pass ends with its findings fixed before the next begins. Non-compliance is a no-go.

### Commit Workflow for CHANGELOG.md

When preparing a commit that includes work tracked in `DONE.md`:

1. **Prepare** — Move all entries from `DONE.md` into `CHANGELOG.md` under a new heading using the date (e.g., `## 2026-06-17 — short title`). Clear the unreleased section in `DONE.md`.
2. **Verify BEFORE committing** — Confirm CHANGELOG.md contains the date entry by running: `grep -c "## YYYY-MM-DD" CHANGELOG.md` (replace with actual date). If grep finds zero matches, STOP and fix CHANGELOG.md before proceeding. Code changes MUST NOT be committed without their changelog entry in the same commit.
3. **Commit** — Stage all files (`git add -A`) and commit with full changelog as commit message body: `git commit -m "short summary\n\nfull changelog body"`. The commit MUST include CHANGELOG.md (date heading) AND DONE.md (cleared). If the commit fails (e.g., pre-commit hook rejection or conflict), do NOT proceed to step 4. Report the exact error output to the user and stop. Do not attempt to resolve hooks or conflicts autonomously.
4. **Get hash** — Run `git --no-pager log -1 --oneline` to retrieve the new commit SHA.
5. **Update CHANGELOG.md** — Replace the date placeholder in the heading with the actual SHA (e.g., `## b4529c9 (2026-06-17) — short title`). If updating CHANGELOG.md fails, report the error and the SHA to the user so they can apply the fix manually. Do not attempt to amend or re-commit under any circumstances.
6. **STOP** — Do NOT amend, do NOT recommit, do NOT stage/commit CHANGELOG.md again. The hash is now baked in and the commit chain is clean.

> ⚠️ NEVER amend a commit to fix the SHA in CHANGELOG.md — amending changes the hash, creating an infinite loop.
>
> **Hard constraint**: A code change commit that skips step 1 (writing DONE.md entries into CHANGELOG.md) produces a broken commit chain where code and its changelog are separated across commits. This is always a structural error. If you discover such a mistake after committing, fix it by soft-resetting to the parent (`git reset --soft HEAD~1`), adding the CHANGELOG entry with date placeholder, then re-committing as step 3 — this restores the invariant that code + changelog live in one commit.

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
