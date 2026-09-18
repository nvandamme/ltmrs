# Model manifest — intfloat/multilingual-e5-small (WP-06)

**Status:** PINNED (reference recipe; qualification fixtures generated 2026-09-18)
**Design ref:** plans/01_design_and_concepts.md §9.1–9.3, RV-09

## Identity and revision

| Field | Value |
|---|---|
| Model ID | `intfloat/multilingual-e5-small` |
| Revision (main HEAD at pin time) | `614241f622f53c4eeff9890bdc4f31cfecc418b3` |
| Source repo | https://huggingface.co/intfloat/multilingual-e5-small |
| License (cardData) | MIT — redistribution permitted; weights are not bundled, fetched explicitly by the artifact manager and verified against this manifest |

## Pinned file digests (SHA-256)

Fetched 2026-09-18 from `resolve/main`. These are the ONLY acceptable digests for
the production cache; any mismatch is a hard error.

| File | SHA-256 | Bytes |
|---|---|---|
| config.json | 69137736cab8b8903a07fe8afaafdda25aac55415a12a55d1bffa9f581abf959 | 655 |
| tokenizer_config.json | a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b | 443 |
| special_tokens_map.json | d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7 | 167 |
| tokenizer.json | 0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39 | 17,082,730 |
| sentencepiece.bpe.model | cfc8146abe2a0488e9e2a0c56de7952f7c11ab059eca145a0a727afce0db2865 | 5,069,051 |
| model.safetensors | 1a55775f53449dac10a2bcbc312469fac40b96d53198c407081a831f81c98477 | 470,641,600 |
| 1_Pooling/config.json | 987f7a67a38fa564c849bb5d277c52ab9088a84368fc0be31a354125aebb12a0 | 200 |
| modules.json | c6e29747481e8b5dd2b58401966aeac910de39092f90cda9a704b1545f902b04 | 387 |

## Architecture recipe (from pinned artifacts, not assumption)

- **Architecture:** `BertModel`, `model_type=bert` (config.json `architectures`).
  This is the RV-09 trap: XLM-RoBERTa *tokenizer* with a BERT *backbone*.
- hidden_size 384 → output dim 384; layers 12; heads 12; intermediate 1536.
- max_position_embeddings **512** (the model window).
- **Tokenizer:** `XLMRobertaTokenizer` — SentencePiece BPE, vocab 250037,
  `clean_up_tokenization_spaces=true`. Special IDs from the live tokenizer:
  pad=**1**, eos/sep=**2**, cls/bos=**0**, unk=**3**. (NOT WordPiece defaults.)
- **Prefix recipe:** query → `"query: "`, document/passage → `"passage: "`;
  applied to all languages including non-English.
- **Pooling:** attention-mask-aware mean over `last_hidden_state`
  (`1_Pooling/config.json`: `pooling_mode_mean_tokens=true`).
- **Normalization:** L2 normalize after pooling (modules.json step 3).
- **dtype:** CPU F32 (torch_dtype float32; CUDA is a separate, later profile).

## Reference environment (fixture generation)

| Field | Value |
|---|---|
| Toolchain | Python 3.13 via `uv venv` (`tmp/e5-artifacts/refenv`) |
| transformers | 4.36.0 (CPU torch, no GPU extensions) |
| Generator script | `tmp/e5-artifacts/gen_reference.py` (dev-only tooling; not shipped in the release binary) |
| Host | Linux x86_64, AVX2 available, 32 cores |

## Reference fixtures

- **File:** `src/embeddings/fixtures/reference_fixture.json` (tracked).
- **Cases:** empty string, short EN query, FR query, code snippet, padded
  mixed-length batch, prefix asymmetry (query vs passage on identical text),
  near-limit 512-token input, long-document chunking with the fact beyond the
  first window.
- **Tolerance rationale:** CPU F32 Candle vs PyTorch F32 reference — elementwise
  cosine similarity ≥ 0.999 and max abs diff ≤ 1e-3 per vector (frozen in tests).
  These are qualification tolerances, not performance claims; any change to the
  recipe or artifacts invalidates them and requires re-running this section.
