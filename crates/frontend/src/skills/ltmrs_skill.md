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
6. **Learn** (`learn`): capture only after solving, and only insight likely
   to pay off again — a non-obvious fix, a discovered convention, a workflow
   that worked. One strong reusable lesson beats several vague ones. A
   `guide` object belongs only on a repeatable procedure worth a `SKILL.md`,
   never on a fact.

## Recall vs reflect vs retain

- **Recall** (`memory_read`, `semantic_search`) returns specific entries.
  Use proactively before questions about past work, user preferences,
  project decisions, or any topic where prior context improves accuracy.
  When in doubt, recall first.
- **Reflect** (`reflect`) synthesizes a coherent answer across many memories.
  Use for open-ended questions spanning stored facts ("What do you know
  about X?", "Summarize project decisions"). `context` focuses the
  synthesis on one angle.
- **Retain** (`memory_add`): durable reusable knowledge only — preferences,
  decisions, architectural choices. Never ephemeral task state. Each
  fragment self-contained: who, what, when, why. Batch related facts
  per call.

## Edit with care (`memory_edit`)

- Read the full fragment before every `update`: recall previews may be
  clipped, and `update` replaces content wholesale — updating from a
  preview deletes the unseen tail.
- Prefer `invalidate` for stale knowledge whose history may stay useful.
  Use `forget` only for content requiring hard deletion.

## Managed skills (`manage_skill`)

- No frontmatter in `body`; frontmatter is generated from `name` and
  `description`.
- `create` fails if the skill exists; `update` fails if it is absent.
  Neither ever touches user-authored skills.

## Distill targets

Worth distilling into guides (`guide_distill`): durable user preferences
(coding style, tooling, communication), project conventions (build, test,
release, review — only what is explicit and enforced), and stable
architectural decisions with recorded rationale. Exclude transient plans,
unresolved ideas, and active task state.

## Hooks (opt-in)

`ltmrs install-hooks` enables `hook-inject`/`hook-extract` for passive
recall/capture. Explicit `memory_*`/`session_*` tools stay primary; hooks
fail open and add nothing when disabled.

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
