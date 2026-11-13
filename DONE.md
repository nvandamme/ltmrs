# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`.

---

## Unreleased Commit
### Live-DB oracle for backup_import_lemma (f6d1a7f, committed without CHANGELOG entry — backfilled here)
- Enveloped copy of live ~/.lemma DB (1457 memories) through real cdylib + preview/confirm; digest-of-emitted-text fix; never-dlclose handle cache. Matrix executed_passed.

### Transitive alias-equivalence oracles (c2f89bb, committed without CHANGELOG entry — backfilled here)
- memory_edit update/forget/invalidate + learn lesson-side alias==native proven by 3 twin-fixture tests; reflect confirmed native (no transitive path).

### Burn CUDA BEIR + batched forward (3b3cbc2, 0a10fc6 — no tracker update at commit time, recorded here)
- LTMRS_BEIR_DEVICE=cuda: 5183 docs 123s index on RTX 5090, identical verdict to CPU. Batched [B,S,H] forward + batched projector publish: 614.7s flat index, byte-identical metrics.

### Candle→Burn staleness in plans (found 2026-11-13 cross-check, NOT edited — plan-authority)
Tree is Candle-free (2ccf544; Cargo.lock + embeddings manifest: 0 candle refs; Burn 0.22 live) but plans still say Candle:
- TODO AD-02 line + Part I §13 table (01:494): pins candle-core 0.11.0
- Part II S4 row (02:23), WP-06 title+body (02:271,283), priority list (02:483)
- Part III T-EMB-01 (03:107), e2e latency (03:274), §10 Q&A (03:439)
- Only Burn mention in plans: my 02:447 BEIR line
Fix needs one review: Part I §13 + AD-02 + WP-06 + affected test rows + deviation ledger. Left for owner call.
