# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`.

---

## Unreleased Commit

### WP-09 remainings: dense guide candidates + differential workflow replay (uncommitted)

- Dense guide-catalog leg (`QueryEmbedder::embed_passages` defaulted trait
  method; `ServiceQueryEmbedder` Passage-role batch override over the worker;
  `SearchBackend::embed_query_sync`/`embed_passages_sync` bridges;
  `guide_catalog_text` + `suggest_guides_dense` + wiring in `exec_guide_get`):
  cosine-ranked (ranking::cosine, >0.0 only) catalog guides append after the
  token matches (cap 5); any failure degrades to byte-identical token output;
  scores never displayed. Role separation enforced by types (passages never
  through the Query seam).
- Differential workflow replay (8-step recall -> act -> persist):
  `tools/capture_workflow.mjs` (synthetic fixture script over MCP stdio) +
  `tools/normalize_workflow.mjs` (placeholder normalization, tracked, byte-
  reproduces the fixture) + `tests/compat/lemma_0_21_0/workflow_fixture.json`
  (pinned upstream transcript + provenance + regen commands) +
  `differential_workflow_replay_matches_upstream` (data-driven replay, exact
  parity on adds/guides, structural sets/deltas with declared divergences:
  4 seed fragments, coaching blocks, read ordering, auto-detected techs,
  distill suggestion, cwd-derived projects).
- Notable upstream behaviors observed (recorded, not replicated): fuzzy
  duplicate refusal on near-identical adds; auto-link on add; seed catalog;
  duplicate guide entry quirk.
- Reviews: formal (data-driven replay args; tracked normalizer; byte-exact
  fixture reproduction) + functional (regex $$ replacements; frozen-clock
  1970 dates; (global) key pass-through; spawn_blocking for block_on;
  type_complexity alias; GuideCreateArgs has no Default).
- Validation: `cargo fmt -- --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test`:
  476 lib + stdio_smoke (1) + vis_smoke (1) passed, 0 failed.
