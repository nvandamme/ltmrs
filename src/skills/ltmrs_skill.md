# ltmrs — Long Term Memory

Use this skill for durable cross-session knowledge: facts, lessons, guides,
and attempt histories. The agent reasons; ltmrs stores explicit findings.

## Workflow: recall → act → persist

1. **Recall** (`memory_read`, `semantic_search`): load what is already known
   before acting. Never re-derive saved facts. Filter by project; global
   scope only when explicitly requested.
2. **Act**: do the task with the recalled context.
3. **Persist** (`memory_add`): save durable, reusable findings immediately —
   decisions, gotchas, patterns. One idea per fragment, 30–2000 chars,
   structured markdown, always in English.
4. **Practice** (`guide_practice`): record guide usage and outcome.
5. **Record** (`session_attempt`/`session_end`): abandoned approaches are the
   most valuable records — outcome `rejected` with the reason.

## Guides

Reusable procedures distilled from experience (`guide_distill` promotes a
proven pattern/lesson). Prefer existing guides over reinventing
workflows; record each guide use with its outcome.

## Rules

- Store secrets never: redact before persisting.
- Never silently transform `confirm=true` content.
- Sessions are per-channel; never assume another channel's state.
- Retrieval scores are relevance signals, not truth probabilities.

## Multilingual recall

Memory fragments are stored in English for retrieval. Queries in French
match through multilingual recall (E5-small, evidenced on a French→English
fixture case); German, Dutch and other languages are untested objectives,
not supported claims. Identifiers, paths and
code tokens are designed to match verbatim regardless of language.
