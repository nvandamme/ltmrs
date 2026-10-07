# Workspace Crate Split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the single `ltmrs` Cargo package (~52k LOC) into a Cargo workspace of focused crates so an edit in one subsystem rebuilds only that crate and its reverse dependents instead of the whole tree.

**Architecture:** Bottom-up extraction following the existing module DAG (domain → service/compat → embeddings → search → interchange → daemon → frontend → binary). The `search`↔`retrieval` prod cycle and the `search`↔`embeddings` test-only bridge stay inside one crate each; cross-cutting test fixtures that would create dev-cycles are re-homed to repo-only helpers. One crate (or deletion) per commit, full suite green after each.

**Tech Stack:** Rust edition 2024, Cargo workspace with shared root `Cargo.lock`, path dependencies, existing pinned toolchain (`rust-toolchain.toml`).

## Global Constraints

- `cargo fmt -- --check` clean after every task.
- `cargo clippy --all-targets -- -D warnings` clean after every task.
- One reviewed, coherent commit per task (repo rule: no commit sprees, never amend published history).
- No product-plan change: `plans/` WPs, the frozen Lemma wire contract, and all receipt/durability semantics stay identical; only crate paths move.
- Single `Cargo.lock` at the workspace root; dependency versions do not change (move pins verbatim).
- No `unsafe` additions; no new external dependencies.
- Every task ends with the full suite green on the dev profile before committing.

---

## Measured baseline (this machine, dev profile with thin LTO)

- Full `cargo test --tests` (lib 712 + 6 + 4 + 1 + 1): ~3–4 min wall, dominated by single-crate codegen of ~52k LOC plus test-binary links.
- Any `src/` touch rebuilds the entire crate (observed 2m41s–3m29s `cargo test --lib` after single-file edits).
- Module sizes (LOC): daemon 16155, service 8665, retrieval 5203, search 5134, domain 3682, frontend 3292, embeddings 3016, bench 2864, interchange 2218, storage 1709 (zero references), compatibility 1671, skills 892, visualizer 826.
- Heavy compile drivers: `lancedb`/`lance-index`/arrow (search), `candle-core`/`candle-nn`/`tokenizers` (embeddings), `fjall` (service), `rmcp` (frontend only).

## Crate DAG (final state, acyclic)

```text
ltmrs-domain ──┬──► ltmrs-service ──┬──► ltmrs-search ──┬──► ltmrs-daemon ──► ltmrs-frontend ──► ltmrs (bin)
               │                    │    (search+       │      (+ tokio,         (+ rmcp,
               │                    │     retrieval)    │       libc)            tokio)
               │                    │         ▲         │
               │                    │    ltmrs-         │
               │                    │    embeddings ────┘
               │                    │    (candle…)
               │                    │
               ├──► ltmrs-compat ───┼──► ltmrs-interchange ──► ltmrs-daemon
               │    (no heavy deps) │    (backup/restore)
               │                    │
               └────────────────────┴──► ltmrs-daemon
```

- `ltmrs-domain`: `src/domain/` + `src/error.rs`. Deps: serde, serde_json, uuid, thiserror. Nothing else.
- `ltmrs-service`: `src/service/`. Deps: domain, fjall, serde, serde_json, uuid, thiserror.
- `ltmrs-compat`: `src/compatibility/`. Deps: domain, serde, serde_json. (`skills/` stays in the binary; see Task 11.)
- `ltmrs-embeddings`: `src/embeddings/`. Deps: domain, candle-core, candle-nn, tokenizers, reqwest, tokio, tracing, serde. Its two `crate::search` uses are `#[test]`-only; tests gain a dev-dependency on `ltmrs-search` (dev-deps may point upward; prod deps stay acyclic).
- `ltmrs-search`: `src/search/` + `src/retrieval/` (the prod `search`↔`retrieval` cycle cannot be cut without a trait-extraction refactor — explicitly out of scope). Deps: domain, service, embeddings, lancedb, lance-index, tokio, serde. `src/projection/mod.rs` (1 line, check content first) folds here if it is search-related, else domain.
- `ltmrs-interchange`: `src/interchange/`. Deps: domain, service, compat. Its `crate::daemon` uses are `#[test]`-only fixtures (backup.rs test `setup()` drives `Dispatcher`); Task 8 re-homes them to repo-only helpers so no dev-cycle is needed at all.
- `ltmrs-daemon`: `src/daemon/`. Deps: domain, service, compat, embeddings, search, interchange, tokio, libc, tracing, serde, serde_json, uuid, sha2.
- `ltmrs-frontend`: `src/frontend/` + `src/cli.rs` + `src/config.rs`. Deps: domain, service, compat, daemon, embeddings, rmcp, tokio, tracing, serde, serde_json. (`config.rs`/`error.rs` are doc-only 1-line stubs today; fold their future content into the crate that owns it, do not create micro-crates.)
- `ltmrs` (binary, root package): `src/main.rs` + `src/skills/` + `src/visualizer/` + `src/bench/` as binary-local modules. Deps on all lib crates. Integration tests in `tests/` keep working unchanged (they exercise the built binary over IPC). `tools/gen_release_evidence.sh` already passes `--workspace --all-targets` and needs no change.
- `src/storage/` (1709 LOC, `pub mod` in lib.rs is its only repo-wide reference — verified with `grep -rn "crate::storage\|storage::lance\|storage::fjall\|storage::schema\|mod storage" src/ tests/`): DELETE in Task 5 after re-verifying the zero-ref proof on a clean tree.

## Interface contracts between crates

- All cross-crate paths keep their Rust paths with the crate segment swapped: `crate::domain::memory::Memory` becomes `ltmrs_domain::memory::Memory`, etc. No item renames, no signature changes, no visibility changes (keep `pub` as-is; tighten later, never in this plan).
- `lancedb::arrow` re-export discipline stays inside `ltmrs-search` (the only crate that may name it).
- Each crate gets `src/lib.rs` re-exporting its modules exactly as today (`pub mod ...;` in the same order).

---

### Task 0: Lock the baseline numbers

**Files:**
- Create: `reports/crate-split-baseline.txt` (gitignored scratch, not committed)
- Modify: none

**Interfaces:**
- Consumes: clean tree at current HEAD
- Produces: timing table every later task compares against

- [ ] **Step 1: Record a clean-checkout build time**

```bash
git --no-pager status --short
cargo clean -p ltmrs 2>/dev/null; /usr/bin/time -v cargo check --tests 2>&1 | tail -n 3
```

Expected: wall time noted into `reports/crate-split-baseline.txt` (expect several minutes; dependency artifacts in `~/.cargo` stay cached, only `ltmrs` itself rebuilds).

- [ ] **Step 2: Record incremental leaf-touch and hub-touch times**

```bash
touch src/domain/memory.rs && /usr/bin/time -f "%es" cargo check --lib 2>&1 | tail -n 1
touch src/daemon/tools.rs && /usr/bin/time -f "%es" cargo check --lib 2>&1 | tail -n 1
```

Expected: both numbers nearly identical (single crate — this equality IS the problem statement); append both to the baseline file.

- [ ] **Step 3: Record test-link cost**

```bash
touch src/daemon/tools.rs && /usr/bin/time -f "%es" cargo test --lib --no-run 2>&1 | tail -n 1
```

Expected: noted; this is the per-edit tax the split must shrink for subsystem work.

- [ ] **Step 4: Commit nothing (measurements only)**

No commit. Report the three numbers in the task handoff.

---

### Task 1: Workspace scaffolding, zero code movement

**Files:**
- Create: `Cargo.toml.workspace` content merged into root `Cargo.toml` (see steps)
- Modify: root `Cargo.toml` (add `[workspace]`, keep `[package]`), no `src/` moves

**Interfaces:**
- Consumes: baseline from Task 0
- Produces: `cargo check --workspace` green with byte-identical build plan (`cargo tree` diff empty)

- [ ] **Step 1: Add the workspace table to the root manifest**

Add exactly this (resolver pinned so the lockfile stays valid):

```toml
[workspace]
resolver = "2"
members = ["."]
```

Keep `[package]`, all `[dependencies]`, all profiles untouched.

- [ ] **Step 2: Verify the build plan is unchanged**

```bash
cargo tree --depth 1 --prefix none > /tmp/tree-before.txt
cargo check --workspace 2>&1 | tail -n 2
cargo tree --depth 1 --prefix none > /tmp/tree-after.txt
diff /tmp/tree-before.txt /tmp/tree-after.txt && echo "PLAN-IDENTICAL"
```

Expected: `PLAN-IDENTICAL`, check clean.

- [ ] **Step 3: Run the fast gate**

```bash
cargo fmt -- --check && echo FMT-OK
cargo clippy --all-targets -- -D warnings 2>&1 | tail -n 2
cargo test --lib -- service::repository::tests::export_full_covers_canonical_sessions 2>&1 | tail -n 3
```

Expected: fmt clean, clippy clean, 1 test green (proves the harness still runs; the full suite runs from Task 2 on).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml
git commit -m "chore: declare cargo workspace (single member, plan-identical)"
```

---

### Task 2: Extract `ltmrs-domain`

**Files:**
- Create: `crates/domain/Cargo.toml`, `crates/domain/src/lib.rs`
- Move: `src/domain/*` → `crates/domain/src/*` (byte-identical, `git mv`)
- Modify: root `Cargo.toml` (add member + path dep where needed — nothing needs it yet), `src/lib.rs` (remove `pub mod domain;`, add `pub use ltmrs_domain as domain;` ONLY if intra-crate `crate::domain::` paths must keep working — preferred: update all `crate::domain::` to `ltmrs_domain::` in the same commit; ~40 sites, mechanical)

**Interfaces:**
- Consumes: workspace from Task 1
- Produces: `ltmrs-domain` compiling alone; rest of tree resolving `domain` through it

- [ ] **Step 1: Create the crate manifest (deps: serde, serde_json, uuid, thiserror — versions copied verbatim from root)**

```toml
[package]
name = "ltmrs-domain"
version = "0.1.0-alpha"
edition = "2024"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = { version = "1", features = ["preserve_order"] }
uuid = { version = "1", features = ["v4", "v5", "v7", "serde"] }
thiserror = "2"
```

- [ ] **Step 2: Move the sources and rewire paths**

```bash
mkdir -p crates/domain/src
git mv src/domain/clock.rs src/domain/command.rs src/domain/export.rs src/domain/graph.rs src/domain/guide.rs src/domain/id.rs src/domain/interpreter.rs src/domain/legacy.rs src/domain/memory.rs src/domain/mod.rs src/domain/project.rs src/domain/projection.rs src/domain/relation.rs src/domain/session.rs crates/domain/src/
```

Then `crates/domain/src/lib.rs` contains the old `mod.rs` body (module list). In the root crate, replace every `crate::domain::` with `ltmrs_domain::` (verify count first: `grep -rn "crate::domain" src/ --include="*.rs" | wc -l`) and add `ltmrs-domain = { path = "crates/domain" }` to root `[dependencies]`. Fold `src/error.rs` (doc-only stub) out: delete the file and its `pub mod error;` line.

- [ ] **Step 3: Verify alone, then together**

```bash
cargo check -p ltmrs-domain 2>&1 | tail -n 2
cargo check --workspace 2>&1 | tail -n 2
cargo test -p ltmrs-domain 2>&1 | tail -n 3
```

Expected: domain crate checks alone in seconds; workspace green; domain unit tests pass in place (they moved with the code).

- [ ] **Step 4: Time the win and commit**

```bash
touch crates/domain/src/memory.rs && /usr/bin/time -f "%es" cargo check -p ltmrs-domain 2>&1 | tail -n 1
git add -A && git commit -m "refactor: extract ltmrs-domain crate (leaf, zero behavior change)"
```

Expected: domain-only check is seconds vs minutes for the old full crate. Note both numbers in the handoff.

---

### Task 3: Extract `ltmrs-service`

**Files:**
- Create: `crates/service/Cargo.toml`, `crates/service/src/lib.rs`
- Move: `src/service/*` → `crates/service/src/*`
- Modify: root `Cargo.toml` (member + `ltmrs-service` path dep), all `crate::service::` → `ltmrs_service::` in remaining root code

**Interfaces:**
- Consumes: `ltmrs-domain` from Task 2
- Produces: service crate (repository + migrations + faithful fault injector) compiling with only domain + fjall + light deps

- [ ] **Step 1: Create the manifest**

```toml
[package]
name = "ltmrs-service"
version = "0.1.0-alpha"
edition = "2024"

[dependencies]
ltmrs-domain = { path = "../domain" }
fjall = { version = "3.1.10" }
serde = { version = "1", features = ["derive"] }
serde_json = { version = "1", features = ["preserve_order"] }
uuid = { version = "1", features = ["v4", "v5", "v7", "serde"] }
thiserror = "2"
```

Copy the `fjall` version verbatim from the root manifest; afterwards remove `fjall` from root deps ONLY if no remaining root module names it (verify: `grep -rn "fjall::" src/ --include="*.rs" | grep -v "^src/service"` must be empty — `src/storage/` still does, but Task 5 deletes it; until then root keeps the dep).

- [ ] **Step 2: Move, rewire, verify**

```bash
mkdir -p crates/service/src
git mv src/service/migrations.rs src/service/mod.rs src/service/repository.rs src/service/repository_internal.rs crates/service/src/
```

Same rewire pattern as Task 2 (`crate::service::` → `ltmrs_service::`, `crates/service/src/lib.rs` = old `mod.rs` body).

```bash
cargo check -p ltmrs-service 2>&1 | tail -n 2
cargo test -p ltmrs-service 2>&1 | tail -n 3
cargo check --workspace 2>&1 | tail -n 2
```

Expected: all green; service tests (repository suite incl. restore/receipt/namespace tests) pass inside their own crate binary.

- [ ] **Step 3: Time and commit**

```bash
touch crates/service/src/repository.rs && /usr/bin/time -f "%es" cargo check -p ltmrs-service 2>&1 | tail -n 1
git add -A && git commit -m "refactor: extract ltmrs-service crate (repository, zero behavior change)"
```

---

### Task 4: Extract `ltmrs-compat`

**Files:**
- Create: `crates/compat/Cargo.toml`, `crates/compat/src/lib.rs`
- Move: `src/compatibility/*` → `crates/compat/src/*`
- Modify: root manifest + `crate::compatibility::` → `ltmrs_compat::` rewire

**Interfaces:**
- Consumes: `ltmrs-domain`
- Produces: frozen Lemma wire/tools crate with no heavy deps (verify with `cargo tree -p ltmrs-compat --depth 1`)

- [ ] **Step 1: Create the manifest (domain + serde + serde_json + uuid, versions verbatim)**

- [ ] **Step 2: Move, rewire, verify no heavy deps leak in**

```bash
mkdir -p crates/compat/src && git mv src/compatibility/* crates/compat/src/
cargo tree -p ltmrs-compat --depth 1 --prefix none
cargo test -p ltmrs-compat 2>&1 | tail -n 3
cargo check --workspace 2>&1 | tail -n 2
```

Expected: `cargo tree` shows only domain + serde-family + uuid; suite green.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "refactor: extract ltmrs-compat crate (frozen wire, zero behavior change)"
```

---

### Task 5: Delete the unreferenced `src/storage/` module

**Files:**
- Delete: `src/storage/` (1709 LOC) + `pub mod storage;` line in `src/lib.rs`
- Modify: root `Cargo.toml` (drop `fjall` ONLY if Task 3 left it solely for storage — re-verify with grep; `lance-index` has no other users either — same check)

**Interfaces:**
- Consumes: service extraction (proves the canonical store does not need it)
- Produces: smaller tree, fewer compiled lines per workspace check

- [ ] **Step 1: Re-prove zero references on the current tree**

```bash
grep -rn "crate::storage\|storage::lance\|storage::fjall\|storage::schema\|mod storage" src/ tests/ --include="*.rs" | grep -v "^src/storage/"
```

Expected: exactly one line — `src/lib.rs:pub mod storage;`. If anything else appears, STOP and re-scope (the module gained a user).

- [ ] **Step 2: Delete and drop now-unused pins**

```bash
git rm -r src/storage
```

Edit `src/lib.rs` to remove the `pub mod storage;` line. Then check each heavy pin: `grep -rn "lance_index\|lancedb::" src/ crates/ --include="*.rs"` — if `lance-index` has no remaining user, remove it from root deps; same for any dep that was storage-only.

- [ ] **Step 3: Verify and commit**

```bash
cargo check --workspace 2>&1 | tail -n 2
cargo test --workspace --lib 2>&1 | grep -E "test result" | head -n 8
git add -A && git commit -m "chore: delete unreferenced storage module (backend-comparison leftover)"
```

---

### Task 6: Extract `ltmrs-embeddings`

**Files:**
- Create: `crates/embeddings/Cargo.toml`, `crates/embeddings/src/lib.rs`
- Move: `src/embeddings/*` → `crates/embeddings/src/*`
- Modify: root manifest + `crate::embeddings::` → `ltmrs_embeddings::` rewire

**Interfaces:**
- Consumes: `ltmrs-domain`; dev-only bridge to `ltmrs-search` (see Step 2)
- Produces: candle/tokenizers isolated — touching daemon code no longer recompiles them

- [ ] **Step 1: Create the manifest (domain + candle-core + candle-nn + tokenizers + reqwest-rustls + tokio + tracing + serde, versions verbatim)**

- [ ] **Step 2: Move, rewire, add the test-only bridge dev-dependency**

```bash
mkdir -p crates/embeddings/src && git mv src/embeddings/* crates/embeddings/src/
```

The two `crate::search::` uses in `e5_small.rs` tests become `ltmrs_search::` under a `[dev-dependencies] ltmrs-search = { path = "../search" }` entry (create the `crates/search/` shell with manifest + lib.rs in the SAME commit so the path resolves; its modules arrive in Task 7 — an empty lib with `pub mod placeholder` removed in Task 7 is acceptable scaffolding ONLY here because Task 7 is the immediate next commit; call this out in the commit message).

- [ ] **Step 3: Verify, time, commit**

```bash
cargo check -p ltmrs-embeddings 2>&1 | tail -n 2
cargo test -p ltmrs-embeddings 2>&1 | tail -n 3
touch src/daemon/tools.rs && /usr/bin/time -f "%es" cargo check --lib 2>&1 | tail -n 1
git add -A && git commit -m "refactor: extract ltmrs-embeddings crate (candle isolated, zero behavior change)"
```

Expected: daemon touch no longer rebuilds candle/transformer codegen (compare with Task 0 numbers).

---

### Task 7: Extract `ltmrs-search` (search + retrieval together)

**Files:**
- Fill: `crates/search/src/{search,retrieval}/` (replace Task 6 placeholder), `crates/search/src/lib.rs`, `crates/search/Cargo.toml`
- Move: `src/search/*`, `src/retrieval/*`; `src/projection/mod.rs` folds here if search-related (read its single line first — if it belongs to domain, move it to `crates/domain/src/` instead)
- Modify: root manifest + `crate::search::` / `crate::retrieval::` → `ltmrs_search::` rewire

**Interfaces:**
- Consumes: domain, service, embeddings crates
- Produces: Lance-backed retrieval crate owning the `lancedb::arrow` discipline (no other crate may name arrow types — verify with grep)

- [ ] **Step 1: Move both modules under one crate root**

```bash
mkdir -p crates/search/src
git mv src/search crates/search/src/search
git mv src/retrieval crates/search/src/retrieval
```

`crates/search/src/lib.rs` declares `pub mod search; pub mod retrieval;`. Internal `crate::search::` / `crate::retrieval::` become `crate::search::` / `crate::retrieval::` (unchanged — they are intra-crate now); only EXTERNAL users rewire to `ltmrs_search::`.

- [ ] **Step 2: Manifest (domain, service, embeddings, lancedb, lance-index if still pinned, tokio, serde) and arrow check**

```bash
grep -rn "arrow" crates/ --include="*.rs" | grep -v "ltmrs-search\|lancedb::arrow" | head
cargo test -p ltmrs-search 2>&1 | tail -n 3
cargo check --workspace 2>&1 | tail -n 2
```

Expected: no arrow path outside `ltmrs-search`; suite green.

- [ ] **Step 3: Time and commit**

```bash
touch crates/search/src/retrieval/engine.rs && /usr/bin/time -f "%es" cargo check -p ltmrs-search 2>&1 | tail -n 1
git add -A && git commit -m "refactor: extract ltmrs-search crate (search+retrieval, zero behavior change)"
```

---

### Task 8: Extract `ltmrs-interchange` (with test-fixture re-homing)

**Files:**
- Create: `crates/interchange/Cargo.toml`, `crates/interchange/src/lib.rs`
- Move: `src/interchange/*` → `crates/interchange/src/*`
- Modify: `crates/interchange/src/backup.rs` test `setup()` (drop `Dispatcher`, build on `CanonicalRepository` directly), root manifest + rewire

**Interfaces:**
- Consumes: domain, service, compat
- Produces: backup/restore crate with NO daemon edge at all (not even dev) — verify with `grep -rn "daemon" crates/interchange/`

- [ ] **Step 1: Re-home the dispatcher test fixtures to repo-only**

In `crates/interchange/src/backup.rs` tests, replace the `Dispatcher`-based `setup()` with direct `CanonicalRepository::open_with_clock` + `issue_namespace` (pattern: `service::repository::tests::repo_with_ns`), seeding memories via `repo.apply` with `AddMemory` commands instead of `add_fragment`/`tool_call`. Keep every assertion identical (manifest counts, digest stability, session cargo = 1 traced session created via `session_start_tx`).

- [ ] **Step 2: Move, manifest (domain, service, compat + tempfile dev-dep), verify the cut**

```bash
mkdir -p crates/interchange/src && git mv src/interchange/* crates/interchange/src/
grep -rn "daemon" crates/interchange/ ; echo "EXPECT-EMPTY"
cargo test -p ltmrs-interchange 2>&1 | tail -n 3
cargo check --workspace 2>&1 | tail -n 2
```

Expected: grep empty; suite green.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "refactor: extract ltmrs-interchange crate (daemon-free, fixtures re-homed)"
```

---

### Task 9: Extract `ltmrs-daemon`

**Files:**
- Create: `crates/daemon/Cargo.toml`, `crates/daemon/src/lib.rs`
- Move: `src/daemon/*` → `crates/daemon/src/*`
- Modify: root manifest + `crate::daemon::` → `ltmrs_daemon::` rewire

**Interfaces:**
- Consumes: domain, service, compat, embeddings, search, interchange
- Produces: the 16k-LOC hub as one crate (inner file splits explicitly out of scope)

- [ ] **Step 1: Manifest (all six lib deps + tokio + libc + tracing + serde + serde_json + uuid + sha2, versions verbatim)**

- [ ] **Step 2: Move, rewire, run the daemon-heavy suites in place**

```bash
mkdir -p crates/daemon/src && git mv src/daemon/* crates/daemon/src/
cargo test -p ltmrs-daemon 2>&1 | tail -n 3
cargo check --workspace 2>&1 | tail -n 2
```

Expected: dispatcher/server/tools/registry tests green inside the crate.

- [ ] **Step 3: Time the flagship win and commit**

```bash
touch crates/domain/src/memory.rs && /usr/bin/time -f "%es" cargo check --lib 2>&1 | tail -n 1
git add -A && git commit -m "refactor: extract ltmrs-daemon crate (hub, zero behavior change)"
```

Expected: a domain touch now rebuilds domain+dependents but the daemon crate itself only re-links if its own sources are untouched… (record the honest number — reverse deps still rebuild; the win is daemon-local edits no longer rebuilding search/embeddings/service codegen).

---

### Task 10: Extract `ltmrs-frontend` and assemble the binary crate

**Files:**
- Create: `crates/frontend/Cargo.toml`, `crates/frontend/src/lib.rs`
- Move: `src/frontend/*`, `src/cli.rs`, `src/config.rs` → `crates/frontend/`
- Keep in root: `src/main.rs` (+ moved-in `skills/`, `visualizer/`, `bench/` as binary-local modules), root `Cargo.toml` keeps `[package name = "ltmrs"]` + bins
- Modify: `src/lib.rs` shrinks to re-exports-or-nothing (if nothing remains, delete `src/lib.rs` and the root `[lib]` section)

**Interfaces:**
- Consumes: all lib crates
- Produces: `cargo run -p ltmrs -- --help` works; `tests/` integration suites (daemon_lifecycle, release_evidence, stdio_smoke, vis_smoke) pass unchanged against the binary

- [ ] **Step 1: Move frontend + CLI surface, manifest (domain, service, compat, daemon, embeddings, rmcp, tokio + light deps, versions verbatim)**

- [ ] **Step 2: Re-home binary-only modules**

Move `src/skills/`, `src/visualizer/`, `src/bench/` into `src/` submodules of the root binary crate (they are never imported by lib code — verify: `grep -rn "crate::skills\|crate::visualizer\|crate::bench" crates/ | head` must be empty; `visualizer` uses `crate::cli` — update to the frontend crate path or move `cli.rs` access accordingly).

- [ ] **Step 3: Verify binary + integration suites**

```bash
cargo run -p ltmrs -- --help 2>&1 | head -n 5
cargo test --test stdio_smoke --test vis_smoke 2>&1 | grep -E "test result"
cargo test --test daemon_lifecycle 2>&1 | grep -E "test result"
```

Expected: help prints; all three suites green.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "refactor: extract ltmrs-frontend crate, assemble binary crate (zero behavior change)"
```

---

### Task 11: Dev-profile LTO experiment (measure, then owner decides)

**Files:**
- Modify: root `Cargo.toml` `[profile.dev]` only, temporarily, then per decision
- Create: nothing (numbers go in the handoff, and permanently in the release-evidence notes if the decision changes the profile)

**Interfaces:**
- Consumes: full workspace from Task 10
- Produces: measured link-time delta + a keep-or-revert recommendation (no code change if kept)

- [ ] **Step 1: Measure with thin LTO (current)**

```bash
touch crates/daemon/src/tools.rs && /usr/bin/time -f "%es" cargo check --tests 2>&1 | tail -n 1
```

- [ ] **Step 2: Measure without LTO and compare**

```bash
# temporarily comment out lto = "thin" under [profile.dev]
touch crates/daemon/src/tools.rs && /usr/bin/time -f "%es" cargo check --tests 2>&1 | tail -n 1
```

Expected: a like-for-like wall-time pair. Present both; the repo owner earlier kept thin LTO deliberately (+1.3% binary size note), so default to KEEP unless the delta exceeds ~30% with no runtime evidence either way. Restore the file to its decided state and commit only if it changed:

```bash
git add Cargo.toml && git commit -m "chore: dev-profile LTO decision (measured X→Y)" # or skip if unchanged
```

---

### Task 12: Final gate, timing report, hygiene rule

**Files:**
- Modify: `AGENTS.md` (extend the Module Organization rule with the crate DAG + acyclicity rule), `DONE.md`/`CHANGELOG.md` per repo commit workflow
- Create: none

**Interfaces:**
- Consumes: all previous tasks
- Produces: committed final state + before/after timing table in the changelog entry

- [ ] **Step 1: Full release evidence on the workspace**

```bash
bash tools/gen_release_evidence.sh --release --locked 2>&1 | tail -n 2
```

Expected: exit 0 with a fresh bundle dir; manifest shows the new HEAD.

- [ ] **Step 2: Before/after timing table**

Re-run the three Task 0 measurements and tabulate against baseline in the CHANGELOG entry body (per-crate checks replace the old full-crate numbers).

- [ ] **Step 3: Update the repo hygiene rule**

In `AGENTS.md`, under Module Organization, append the crate DAG and the rule: new code lives in the owning crate; cross-crate prod deps must keep the DAG acyclic (test-only upward edges via dev-dependencies are allowed but must be called out); `lancedb::arrow` stays inside `ltmrs-search`.

- [ ] **Step 4: Commit per workflow (DONE → CHANGELOG → verify → commit → hash → bake → stop)**

---

## Out of scope (explicitly)

- Splitting `tools.rs` (10.5k) or `repository.rs` (7.5k) into smaller files — follow-up plan, not this one.
- Extracting shared traits to break the `search`↔`retrieval` cycle — measured first; only if the combined crate stays a bottleneck.
- Changing dependency versions, features, or the lockfile.
- `sccache` / remote caching / CI pipeline changes.
- Any product behavior, wire format, receipt, or durability semantic change.
