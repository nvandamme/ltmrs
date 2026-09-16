# AD-01 Canonical backend: Option B (Fjall + Lance)

**Status:** DECIDED
**Decided:** 2026-09-16
**Closes:** AD-01 (plans/01_design_and_concepts.md §13)

## Decision

Canonical state lives in **Fjall keyspaces** (optimistic cross-keyspace
transactions). **Lance** is the search/projection store, not the canonical
source of truth. This is Option B from the WP-02 gate.

## Rationale

The WP-02 hard gates disqualify a backend on atomicity, durability, or
isolation — not on throughput. The probes establish:

- **Fjall** provides optimistic write transactions with SSI conflict
  detection. Concurrent absent-key creates yield exactly one winner;
  write-write conflicts on a shared key are detected at commit; merges are
  atomic (no partial state); durability holds across kill/reopen with
  `persist(SyncAll)`; bounded retries resolve contention without external
  side effects.
- **Lance alone** cannot guarantee a single winner for concurrent
  absent-key creates (merge_insert last-writer-wins), and its
  `set_unenforced_primary_key` is metadata, not a uniqueness constraint.
  It is therefore unsuitable as the sole canonical backend.

Fjall's transactional guarantees map directly onto RQ-01 (atomic commands),
RQ-04 (revision/uniqueness enforcement), and RQ-06 (retry-safe effects).
Lance retains its native strength: vector/FTS search as a derived
projection refreshed after canonical commits.

## Evidence

| Probe | Result | Source |
|---|---|---|
| Lance concurrent absent-key create | NOT one-winner (counterexample) | `lance_backend::tests::concurrent_absent_key_creates_lance_only_is_not_one_winner` |
| Lance unenforced PK | metadata only, not a constraint | `lance_backend::tests::test_unenforced_primary_key_is_metadata_not_constraint` |
| Lance kill/reopen durability | passes | `lance_backend::tests::test_kill_and_reopen_durability` |
| Fjall concurrent absent-key create | one-winner | `fjall_backend::tests::fjall_one_winner_for_concurrent_absent_key` |
| Fjall write-write conflict | detected | `fjall_backend::tests::fjall_detects_write_write_conflict` |
| Fjall atomic merge | no partial state | `fjall_backend::tests::fjall_atomic_merge_no_partial_state` |
| Fjall kill/reopen durability | passes (SyncAll) | `fjall_backend::tests::fjall_kill_reopen_durability_syncall` |
| Fjall bounded retry | contention resolved | `fjall_backend::tests::fjall_bounded_retry_under_contention` |

## Counterexamples considered

- **Option A (Lance-only):** rejected. Concurrent absent-key creates do not
  yield a unique winner; primary-key metadata is not a uniqueness
  constraint. Fails the atomicity/isolation gate.
- **Ad hoc Lance transaction framework:** rejected per WP-02 directive —
  no hand-rolled transaction layer over Lance.

## Configuration

- Fjall: `fjall` 3.1.10 (optimistic transactions, `PersistMode::SyncAll`).
- Lance: `lancedb` 0.38.0 (`features = ["remote"]`, see F-01) as the
  projection/search store.
- Both pinned in `Cargo.toml`; see AD-02 for the full lock.

## Critical pitfall: Fjall `temporary(true)` deletes the data directory on drop

**Data-loss hazard. Read before opening any temporary Fjall database.**

Fjall's `Builder::temporary(true)` sets `clean_path_on_drop = true`. When the
`Database` handle is dropped, Fjall's `Drop` handler runs
`remove_dir_all(config.path)` (fjall `src/db.rs` Drop impl). It logs
`"Deleting database because temporary=true: <path>"` and then **recursively
deletes the entire directory at that path**.

This is correct behavior *by design* for a genuine throwaway directory — but it
is catastrophic if the path is a real, populated directory.

### Incident

An early `CanonicalRepository::open_temporary()` opened Fjall at the current
working directory:

```rust
OptimisticTxDatabase::builder(".").temporary(true).open()?
```

When the test repository was dropped, Fjall executed `remove_dir_all(".")`,
wiping the working directory (source, `Cargo.toml`, everything). The symptom
looked like "Fjall/LanceDB is deleting our data", but Fjall was doing exactly
what it is documented to do — the bug was ltmrs handing it `"."` as the
"temporary" path. Lance was a red herring (its tests use `tempfile::tempdir()`).

### Rules (non-negotiable)

- **Never** pass a real, relative, or CWD path (`"."`, `".."`, `"./data"`, a
  user data dir) to a Fjall builder that also sets `.temporary(true)`.
- Temporary/test databases must use a **unique directory under the OS temp
  dir**, so the on-drop cleanup only removes that isolated directory:
  ```rust
  let path = std::env::temp_dir().join(format!("ltmrs-fjall-{}", uuid::Uuid::now_v7()));
  let db = OptimisticTxDatabase::builder(&path).temporary(true).open()?;
  ```
- `tempfile` is a dev-dependency, so non-test code cannot use it directly; use
  `std::env::temp_dir()` + a unique suffix (e.g. a UUID) instead.
- Production databases use `builder(base_path).open()` with **no**
  `.temporary(true)`, so their data survives drop. Keep it that way.

### Detection

A sentinel file in the working directory (`SENTINEL_CWD_TEST.txt`) is used to
verify that test runs never wipe the CWD. If it disappears after a test run,
a `temporary(true)` database was opened at a real path.

## Retained tests

Backend-independent hard-gate tests (T-STORE, T-CONC, T-REC, T-GATE) are
retained and run against the selected Fjall canonical path. The Lance probe
remains as the search-projection capability evidence.
