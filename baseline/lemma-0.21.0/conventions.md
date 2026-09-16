# ltmrs — Test & Baseline Conventions (WP-00)

Conventions established for reproducible, deterministic testing and baseline
capture. These are inputs to all work packages.

## Sandbox home

- All upstream captures and integration tests run against an isolated HOME,
  never the user's real `~/.lemma` or `~/.agents`.
- Convention: `vendor/lemma-sandbox/home/` for upstream captures; a fresh
  temp HOME per test for integration tests.
- The Lemma server resolves its store from `os.homedir()`:
  - DB: `$HOME/.lemma/lemma.db`
  - Config: `$HOME/.lemma/config.json`
  - Skill: `$HOME/.agents/skills/lemma/SKILL.md`
- Set `HOME` in the test harness; do not rely on env leakage.

## Deterministic clock and IDs

- Tests use a frozen/injected clock. Never read wall-clock time in a
  determinism-sensitive assertion.
- IDs: use a deterministic ID generator (seeded) in tests; UUIDv7 is an
  identity, not a commit-order cursor (Part I 8.2, RV-07).
- Where upstream generates IDs/timestamps, normalize via a bijection or
  injected values; never normalize away field presence or types.

## Fixture conventions

- `tests/fixtures/` holds committed, small, deterministic fixtures.
- Capture outputs (wire transcripts, tool snapshots, upstream-lock) live in
  `baseline/lemma-0.21.0/` and are committed with provenance.
- Generated reports and bulky artifacts go to `reports/` (gitignored).
- Never commit secrets, model weights, or private memories; reference by
  digest/provenance.

## Provenance rule (STRICT)

- Every captured artifact records: commit, versions, timestamps, environment.
- Network/tooling failures leave `not_captured`, never a hand-written golden.
- Source inspection is not execution; a plan statement is not a passing gate.

## Upstream capture utility

- `tools/capture_lemma.mjs` — captures the live MCP wire transcript of the
  pinned upstream in an isolated sandbox. Reproducible; records provenance.
