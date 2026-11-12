# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`

---
## Unreleased Commit

- omp backend-compat P1 (redaction hardening, TDD RED-first):
- Gap: only `fragment` was scanned/redacted; title/description/evidence.snippet reached prompts verbatim. RED test `memory_add_redacts_title_and_description` failed pre-fix (stored raw title).
- Fix: detection joins all text-bearing fields (fragment+title+description+evidence.snippet, omp redactMemoryTextFields parity); generated title/description derive from the redacted fragment; evidence snippets redacted with sha over redacted bytes. JWT-shaped tokens pinned by regression test (already covered by existing patterns).
- Scout: full omp memory facilities mapped (MemoryBackend trait, 5 agent tools, 25 mnemopi MCP tools, sharpshooter friction, hindsight bank scoping); gap table + P2/P3 spec at `plans/2026-11-12-omp-compat-p2-p3-spec.md` (memory_edit tri-op, global-scope mapping, status surfaces).
- WP-12 fold-in: RQ-20 evidence (redaction parity) + TODO omp-compat lines (P1 done, P2/P3 open).
- Proof: daemon memory_add 26/26 incl. 3 redaction tests, compat 46/46; workspace clippy clean; fmt clean; gate-audit 13/13.
