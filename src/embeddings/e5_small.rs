//! The qualified E5-small Candle adapter (WP-06 tasks 2, 3, 10).
//!
//! Implements the pinned reference recipe exactly: XLM-RoBERTa tokenizer,
//! BertModel backbone, query/passage prefixes, attention-mask-aware mean
//! pooling and L2 normalization. Unsupported recipes are rejected with an
//! actionable message — never a best-effort load of arbitrary weights.

use std::path::Path;
use std::sync::Arc;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use tokenizers::{Encoding, Tokenizer};

use crate::domain::id::ModelFingerprint;
use crate::embeddings::artifacts::{ArtifactCache, ArtifactError, ArtifactResult, HttpClient};
use crate::embeddings::bert_impl::{BertConfig, BertModel};
use crate::embeddings::manifest::{ModelRecipe, e5_small_artifact, e5_small_recipe};
use crate::embeddings::recipe::{Normalization, Role};

/// Canonical fingerprint for requests in the E5-small vector space (AD-04:
/// never mix vectors across spaces). Pins the request side to the same
/// namespace the test fixtures and staged generation records already use
/// by value; the production projector must take the same value when it is
/// wired. Changing the model or recipe requires a new value, while
/// chunking-policy changes bump only the chunker version.
pub const E5_SMALL_FINGERPRINT: ModelFingerprint = ModelFingerprint::new(1);

/// A single embedding request: text plus its role (determines the prefix).
#[derive(Debug, Clone)]
pub struct EmbedInput {
    pub text: String,
    pub role: Role,
}

/// One embedded sequence with its tokenization evidence.
#[derive(Debug, Clone)]
pub struct EmbeddedSequence {
    /// L2-normalized vector (dim = recipe.output_dim). Always finite.
    pub vector: Vec<f32>,
    /// Token IDs actually fed to the model (prefix included; T-EMB-01 evidence).
    pub input_ids: Vec<u32>,
    /// Attention mask as consumed by the model, padded to batch length.
    pub attention_mask: Vec<bool>,
}

/// A deterministic derived chunk of a long memory (WP-06 task 6; RV-10).
/// The parent memory keeps its identity/ID; this records the exact span.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Text to embed (already prefixed by the caller's recipe).
    pub text: String,
    /// Character offset of `text` start within the original fragment.
    pub char_start: usize,
    /// Character offset of `text` end (exclusive) within the fragment.
    pub char_end: usize,
    /// Token count after prefixing — always <= recipe.max_tokens.
    pub token_count: usize,
}

/// The E5-small adapter. Synchronous and `&mut self` — owned by the bounded
/// worker, never called from Tokio I/O threads directly (design §9).
pub struct E5SmallAdapter {
    recipe: ModelRecipe,
    tokenizer: Arc<Tokenizer>,
    model: BertModel,
}

impl E5SmallAdapter {
    /// Load from a verified artifact directory (digests checked by the cache).
    /// Returns an actionable error for unsupported recipes instead of trying.
    pub fn load_verified(dir: &Path) -> ArtifactResult<Self> {
        let recipe = e5_small_recipe();

        // Task 10: validate the pinned recipe before touching any weights.
        Self::validate_config_file(dir, &recipe)?;

        let tokenizer_path = dir.join("tokenizer.json");
        let tokenizer =
            Arc::new(Tokenizer::from_file(&tokenizer_path).map_err(|e| {
                ArtifactError::Download(recipe.id.clone(), format!("tokenizer: {e}"))
            })?);

        let weights_path = dir.join("model.safetensors");
        let device = Device::Cpu;
        let weights: std::collections::HashMap<String, Tensor> =
            candle_core::safetensors::load(&weights_path, &device).map_err(|e| {
                ArtifactError::Download(recipe.id.clone(), format!("safetensors: {e}"))
            })?;

        let vb = VarBuilder::from_tensors(weights, DType::F32, &device);

        let config = Self::read_bert_config(dir)?;
        let model = BertModel::load(vb, &config).map_err(|e| {
            ArtifactError::Download(recipe.id.clone(), format!("BertModel load: {e}"))
        })?;

        Ok(Self {
            recipe,
            tokenizer,
            model,
        })
    }

    /// Load strictly from the cache (offline-safe) after verification.
    pub fn load_from_cache(cache: &ArtifactCache) -> ArtifactResult<Self> {
        let artifact = e5_small_artifact();
        Self::load_verified(&cache.load_cached(&artifact)?)
    }

    /// Explicit fetch + verify, then load (online mode).
    #[allow(dead_code)] // used by CLI/daemon wiring in later WPs
    pub async fn fetch_and_load(
        cache: &ArtifactCache,
        client: Option<&HttpClient>,
    ) -> ArtifactResult<Self> {
        let artifact = e5_small_artifact();
        let dir = cache
            .ensure_model(&artifact, client.map(|c| c.as_ref()))
            .await?;
        Self::load_verified(&dir)
    }

    /// Recipe validation (task 10): reject anything that is not the pinned
    /// E5-small recipe with an actionable message. This is what prevents
    /// "trying any arbitrary safetensors model".
    pub fn validate_config_file(dir: &Path, recipe: &ModelRecipe) -> ArtifactResult<()> {
        let config_path = dir.join("config.json");
        let raw = std::fs::read_to_string(&config_path).map_err(|e| {
            ArtifactError::Download(
                recipe.id.clone(),
                format!("cannot read {}: {e}", config_path.display()),
            )
        })?;

        // Architecture must be BertModel (the RV-09 trap: bert backbone +
        // XLM-RoBERTa tokenizer).
        let cfg: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            ArtifactError::Download(
                recipe.id.clone(),
                format!("config.json is not valid JSON: {e}"),
            )
        })?;

        let architectures = cfg
            .get("architectures")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>());
        if architectures != Some(vec!["BertModel"]) {
            return Err(unsupported(format!(
                "config.json 'architectures' must be [\"BertModel\"] for the pinned \
                 E5-small recipe, found: {cfg:?}"
            )));
        }

        let model_type = cfg.get("model_type").and_then(|v| v.as_str());
        if model_type != Some("bert") {
            return Err(unsupported(format!(
                "config.json 'model_type' must be \"bert\" for the pinned E5-small recipe, \
                 found: {cfg:?}"
            )));
        }

        // Tokenizer class must match: XLM-RoBERTa SentencePiece, not WordPiece.
        let tok_path = dir.join("tokenizer_config.json");
        if tok_path.exists() {
            let tok_cfg: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&tok_path).map_err(|e| {
                    ArtifactError::Download(
                        recipe.id.clone(),
                        format!("cannot read tokenizer_config.json: {e}"),
                    )
                })?)
                .map_err(|e| {
                    ArtifactError::Download(
                        recipe.id.clone(),
                        format!("tokenizer_config.json invalid JSON: {e}"),
                    )
                })?;
            if tok_cfg.get("tokenizer_class").and_then(|v| v.as_str())
                != Some("XLMRobertaTokenizer")
            {
                return Err(unsupported(format!(
                    "tokenizer_config.json 'tokenizer_class' must be \"XLMRobertaTokenizer\" \
                     for the pinned E5-small recipe, found: {tok_cfg:?}"
                )));
            }
        }

        Ok(())
    }

    fn read_bert_config(dir: &Path) -> ArtifactResult<BertConfig> {
        // Parse using our custom BertConfig loader (handles all required fields).
        let path = dir.join("config.json");
        BertConfig::load_from_config_file(&path).map_err(|e| {
            unsupported(format!(
                "config.json does not match the BertModel schema ltmrs supports ({e}); \
                 re-pin the model or extend the adapter"
            ))
        })
    }

    pub fn recipe(&self) -> &ModelRecipe {
        &self.recipe
    }

    /// Special token IDs from the live tokenizer artifacts (T-EMB-01).
    /// Returns (pad, eos/sep) resolved by name against the pinned vocab.
    #[allow(dead_code)] // used by qualification tests and later WPs
    pub fn special_token_ids(&self) -> (u32, u32) {
        let pad = self.tokenizer.token_to_id("<pad>").unwrap_or(1);
        let eos = self.tokenizer.token_to_id("</s>").unwrap_or(2);
        (pad, eos)
    }

    /// The padding token ID used to right-pad batches.
    fn pad_token_id(&self) -> u32 {
        self.tokenizer.token_to_id("<pad>").unwrap_or(1)
    }

    /// Tokenize without running the model — used by chunking (task 6) and tests.
    pub fn tokenize(&self, text: &str, role: Role) -> Vec<u32> {
        let prefixed = self.prefixed(text, role);
        self.raw_tokenize(&prefixed)
    }

    /// Tokenize raw text with no prefix (used by chunking to count the exact
    /// model input length).
    fn raw_tokenize(&self, text: &str) -> Vec<u32> {
        self.tokenizer
            .encode(text.to_string(), true)
            .expect("tokenizer is valid")
            .get_ids()
            .to_vec()
    }

    /// Number of tokens for a candidate chunk (prefix + text), no truncation.
    pub fn count_tokens(&self, text: &str, role: Role) -> usize {
        self.tokenize(text, role).len()
    }

    fn prefixed(&self, text: &str, role: Role) -> String {
        let p = match role {
            Role::Query => self.recipe.query_prefix,
            Role::Passage => self.recipe.passage_prefix,
        };
        format!("{p}{text}")
    }

    /// Split a long passage into deterministic chunks that each fit the model
    /// window when prefixed. Units are paragraphs/lines; packing is greedy and
    /// token-counted (never character heuristics). Oversized single units get a
    /// disclosed hard split at the tokenizer boundary. Offsets refer to `fragment`.
    pub fn chunk_passage(&self, title: &str, fragment: &str) -> Vec<Chunk> {
        let max_tokens = self.recipe.max_tokens;

        // Units are non-empty lines as (line_start, line_end). A chunk is always
        // the verbatim fragment span from its first unit's start to its last
        // unit's end, so `fragment[char_start..char_end] == text` holds exactly
        // and blank lines between units stay inside the slice (T-EMB-03).
        let mut units: Vec<(usize, usize)> = Vec::new();
        let mut pos = 0usize;
        for seg in fragment.split_inclusive('\n') {
            let trimmed_end = seg.strip_suffix('\n').unwrap_or(seg);
            if !trimmed_end.trim().is_empty() {
                units.push((pos, pos + trimmed_end.len()));
            }
            pos += seg.len();
        }

        // The bounded prefix: title (bounded) + passage marker. Kept constant so
        // every chunk shares the same leading context tokens.
        let prefix = format!("passage: {title}\n");
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut cur_start: Option<usize> = None;
        let mut cur_end = 0usize;

        for (s, e) in &units {
            let candidate_start = match cur_start {
                Some(x) => x,
                None => *s,
            };
            // Candidate chunk is the verbatim span up to this unit's end.
            let candidate = &fragment[candidate_start..*e];
            if self.raw_tokenize(&format!("{prefix}{candidate}")).len() <= max_tokens {
                if cur_start.is_none() {
                    cur_start = Some(candidate_start);
                }
                cur_end = *e;
            } else {
                // Current chunk is full: flush it, then restart with this unit.
                if let Some(cs) = cur_start.take() {
                    chunks.push(Chunk {
                        text: fragment[cs..cur_end].to_string(),
                        char_start: cs,
                        char_end: cur_end,
                        token_count: 0, // filled below after prefixing
                    });
                }

                // Does this single unit fit on its own?
                let alone = &fragment[*s..*e];
                if self.raw_tokenize(&format!("{prefix}{alone}")).len() <= max_tokens {
                    cur_start = Some(*s);
                    cur_end = *e;
                } else {
                    // Oversized unit: disclosed hard split at the tokenizer limit
                    // (RQ-10). Each slice is verbatim and fits when prefixed.
                    let budget = max_tokens - self.raw_tokenize(&prefix).len();
                    if budget == 0 {
                        // Pathological prefix alone fills the window; disclose as-is.
                        chunks.push(Chunk {
                            text: alone.to_string(),
                            char_start: *s,
                            char_end: *e,
                            token_count: 0, // filled below after prefixing
                        });
                    } else {
                        let mut os = *s;
                        loop {
                            if self
                                .raw_tokenize(&format!("{prefix}{}", &fragment[os..*e]))
                                .len()
                                <= max_tokens
                            {
                                cur_start = Some(os);
                                cur_end = *e;
                                break;
                            }
                            // Largest char-boundary cut where prefix + span fits budget.
                            let mut lo = os;
                            let mut hi = *e;
                            while hi - lo > 1 {
                                let mid_abs =
                                    os + fragment[os..*e].floor_char_boundary((lo + hi) / 2 - os);
                                if self
                                    .raw_tokenize(&format!("{prefix}{}", &fragment[os..mid_abs]))
                                    .len()
                                    <= budget
                                {
                                    lo = mid_abs;
                                } else {
                                    hi = mid_abs;
                                }
                            }
                            // Progress guard: a single token larger than the budget
                            // would stall the search; advance past it verbatim.
                            let cut = if lo > os {
                                lo
                            } else {
                                fragment[os..*e].ceil_char_boundary(1) + os
                            };
                            chunks.push(Chunk {
                                text: fragment[os..cut].to_string(),
                                char_start: os,
                                char_end: cut,
                                token_count: 0, // filled below after prefixing
                            });
                            os = cut;
                        }
                    }
                }
            }
        }
        if let Some(cs) = cur_start.take() {
            chunks.push(Chunk {
                text: fragment[cs..cur_end].to_string(),
                char_start: cs,
                char_end: cur_end,
                token_count: 0, // filled below after prefixing
            });
        }

        // Fill in accurate token counts for every chunk (prefix included).
        for c in chunks.iter_mut() {
            c.token_count = self.raw_tokenize(&format!("{prefix}{}", c.text)).len();
        }

        chunks
    }

    /// Embed a batch of inputs. Pads rows to the max length in the batch with
    /// pad_token_id and attention_mask=0 (T-EMB-02). Returns one vector per input,
    /// each L2-normalized and finite-checked.
    pub fn embed_batch(&mut self, inputs: &[EmbedInput]) -> ArtifactResult<Vec<EmbeddedSequence>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let pad_id = self.pad_token_id();
        let encodings: Vec<Encoding> = inputs
            .iter()
            .map(|i| {
                let prefixed = self.prefixed(&i.text, i.role);
                self.tokenizer
                    .encode(prefixed, true)
                    .expect("tokenizer is valid")
            })
            .collect();

        // Window enforcement per row (chunking upstream; overflow is a bug).
        for enc in &encodings {
            if enc.get_ids().len() > self.recipe.max_tokens {
                return Err(ArtifactError::Download(
                    e5_small_recipe().id.clone(),
                    format!(
                        "input of {} tokens exceeds the {}-token model window; \
                         use chunked embedding",
                        enc.get_ids().len(),
                        self.recipe.max_tokens
                    ),
                ));
            }
        }

        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);
        let device = &self.model.device;

        // Build padded batch.
        let mut input_ids: Vec<u32> = Vec::with_capacity(inputs.len() * max_len);
        let mut masks: Vec<bool> = Vec::with_capacity(inputs.len() * max_len);
        for enc in &encodings {
            let ids = enc.get_ids();
            for &id in ids {
                input_ids.push(id);
                masks.push(true);
            }
            for _ in ids.len()..max_len {
                input_ids.push(pad_id);
                masks.push(false);
            }
        }

        let batch = Tensor::from_vec(input_ids, (inputs.len(), max_len), device)?;
        let token_type_data: Vec<u32> = vec![0u32; inputs.len() * max_len];
        let token_type =
            Tensor::new(&token_type_data[..], device)?.reshape((inputs.len(), max_len))?;
        let attention_mask_f: Vec<f32> = masks.iter().map(|&b| if b { 1.0 } else { 0.0 }).collect();
        let attention_mask = Tensor::from_vec(attention_mask_f, (inputs.len(), max_len), device)?;

        // Dropout is a no-op in candle's eval path — deterministic output.
        let hidden = self
            .model
            .forward(&batch, &token_type, Some(&attention_mask))?;

        // Attention-mask-aware mean pooling: sum(h * mask) / count(mask).
        // Candle's elementwise ops do not broadcast, so use explicit broadcast_* variants.
        let mask_f32 = attention_mask.unsqueeze(2)?;
        let masked_hidden = hidden.broadcast_mul(&mask_f32)?;
        let sums = masked_hidden.sum(1)?;
        let counts = mask_f32.sum(1)?;
        // Guard against divide-by-zero (empty inputs) without changing valid rows.
        let safe_counts = counts.clamp(1.0f32, f32::MAX)?;
        let pooled = sums.broadcast_div(&safe_counts)?;

        // L2 normalization with epsilon guard (recipe.normalization).
        let normed = match self.recipe.normalization {
            Normalization::L2 { epsilon } => {
                let squared_norms = (&pooled * &pooled)?.sum(1)?;
                let norms = squared_norms.powf(0.5)?;
                let safe_norms = norms.clamp(epsilon as f64, f32::MAX as f64)?;
                pooled.broadcast_div(&safe_norms.unsqueeze(1)?)?
            }
        };

        // Finite-value check: NaN/Inf is a hard failure, never stored.
        let values = normed.to_vec2::<f32>()?;
        for row in &values {
            if !row.iter().all(|v| v.is_finite()) {
                return Err(ArtifactError::Download(
                    e5_small_recipe().id.clone(),
                    "model produced non-finite values (NaN/Inf); refusing to embed".into(),
                ));
            }
        }

        let mut out = Vec::with_capacity(inputs.len());
        for i in 0..inputs.len() {
            let enc = &encodings[i];
            // Reconstruct the per-row attention mask (padded).
            let ids_len = enc.get_ids().len();
            let row_mask: Vec<bool> = (0..max_len)
                .map(|j| j < ids_len && enc.get_attention_mask()[j] != 0)
                .collect();

            out.push(EmbeddedSequence {
                vector: values[i].clone(),
                input_ids: enc.get_ids().to_vec(),
                attention_mask: row_mask,
            });
        }

        Ok(out)
    }

    /// Convenience single-input embed (batch of one).
    pub fn embed(&mut self, text: &str, role: Role) -> ArtifactResult<EmbeddedSequence> {
        let inputs = vec![EmbedInput {
            text: text.to_string(),
            role,
        }];
        Ok(self.embed_batch(&inputs)?.pop().unwrap())
    }
}

fn unsupported(reason: String) -> ArtifactError {
    ArtifactError::UnsupportedModel(e5_small_recipe().id.clone(), reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::manifest::{e5_small_artifact, e5_small_recipe};
    use std::path::PathBuf;

    /// Load the reference fixture JSON from the tracked fixtures directory.
    fn load_fixture() -> serde_json::Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/embeddings/fixtures/reference_fixture.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Populate the cache directory with the pinned artifact files so tests can
    /// run offline against real weights. Returns false (skip) when artifacts are
    /// not present in tmp/e5-artifacts (CI without network).
    fn populate_cache(cache: &ArtifactCache, _guard: &tempfile::TempDir) -> bool {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tmp/e5-artifacts");
        if !src.join("model.safetensors").exists() {
            return false;
        }
        let artifact = e5_small_artifact();
        let mdir = cache.model_dir(&artifact);
        std::fs::create_dir_all(&mdir).unwrap();
        for file in artifact.digests.keys() {
            let from = src.join(file);
            if from.exists() {
                std::fs::copy(&from, mdir.join(file)).unwrap();
            }
        }
        true
    }

    fn test_cache() -> (ArtifactCache, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (ArtifactCache::new(dir.path()), dir)
    }

    /// T-EMB-01: tokenizer IDs and special tokens match the pinned reference.
    #[test]
    fn t_emb_01_tokenizer_matches_reference() {
        let (cache, guard) = test_cache();
        if !populate_cache(&cache, &guard) {
            eprintln!("SKIP: artifacts not present");
            return;
        }
        let adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();
        let fixture = load_fixture();

        // Reference fixture special tokens (from tokenizer.json at pin time):
        // pad=1, eos/sep=2, cls/bos=0.
        assert_eq!(adapter.special_token_ids().0, 1, "pad token id");
        assert_eq!(adapter.special_token_ids().1, 2, "eos token id");

        // short_en case: query role on "How to configure Fjall persistence mode?".
        let ref_q: Vec<u32> = fixture["cases"]["short_en"]["input_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_u64())
            .map(|v| v as u32)
            .collect();
        let q_tokenized = adapter.tokenize("How to configure Fjall persistence mode?", Role::Query);
        assert_eq!(
            q_tokenized, ref_q,
            "query tokenization must match reference"
        );

        // FR query case (non-English).
        let ref_fr: Vec<u32> = fixture["cases"]["fr_query"]["input_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_u64())
            .map(|v| v as u32)
            .collect();
        let fr_tokenized = adapter.tokenize("comment configurer la persistance ?", Role::Query);
        assert_eq!(
            fr_tokenized, ref_fr,
            "FR query tokenization must match reference"
        );

        // Code snippet case (passage role).
        let ref_code: Vec<u32> = fixture["cases"]["code_snippet"]["input_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_u64())
            .map(|v| v as u32)
            .collect();
        let code_tokenized = adapter.tokenize("fn main() { println!(\"hello\"); }", Role::Passage);
        assert_eq!(
            code_tokenized, ref_code,
            "code snippet tokenization must match reference"
        );

        // Prefix asymmetry: query vs passage on same text produce different IDs.
        let q_ids = adapter.tokenize("How to configure Fjall persistence mode?", Role::Query);
        let p_ids = adapter.tokenize("How to configure Fjall persistence mode?", Role::Passage);
        assert_ne!(
            q_ids, p_ids,
            "query and passage prefixes must produce different token sequences"
        );

        // Prefix IDs match reference fixture.
        let ref_p: Vec<u32> = fixture["prefix"]["passage_input_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_u64())
            .map(|v| v as u32)
            .collect();
        assert_eq!(
            p_ids, ref_p,
            "passage prefix tokenization must match reference"
        );

        // Model window is 512.
        assert_eq!(adapter.recipe().max_tokens, 512);
    }

    /// T-EMB-02: single and batched embeddings on empty/short/padded inputs;
    /// shapes, finiteness, normalization and reference equivalence.
    #[test]
    fn t_emb_02_embeddings_match_reference() {
        let (cache, guard) = test_cache();
        if !populate_cache(&cache, &guard) {
            eprintln!("SKIP: artifacts not present");
            return;
        }
        let mut adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();
        let fixture = load_fixture();

        // Tolerance rationale (see manifest): CPU F32 Candle vs PyTorch F32.
        const COSINE_TOL: f32 = 0.999;
        const MAX_ABS_DIFF: f32 = 1e-3;

        fn check_vector(actual: &[f32], expected_json: &serde_json::Value, label: &str) {
            let expected: Vec<f32> = expected_json
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_f64())
                .map(|v| v as f32)
                .collect();
            assert_eq!(actual.len(), expected.len(), "{label}: dimension mismatch");

            // Finiteness.
            assert!(
                actual.iter().all(|v| v.is_finite()),
                "{label}: non-finite values"
            );

            // L2 normalization: norm should be ~1.0.
            let norm_sq: f32 = actual.iter().map(|v| v * v).sum();
            assert!(
                (norm_sq - 1.0).abs() < 1e-4,
                "{label}: not unit-normalized (norm^2={norm_sq})"
            );

            // Cosine similarity (both normalized, so dot product = cosine).
            let cos: f32 = actual.iter().zip(expected.iter()).map(|(a, b)| a * b).sum();
            assert!(
                cos > COSINE_TOL,
                "{label}: cosine {cos} below tolerance {COSINE_TOL}"
            );

            // Max absolute difference.
            let max_diff: f32 = actual
                .iter()
                .zip(expected.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0f32, f32::max);
            assert!(
                max_diff < MAX_ABS_DIFF,
                "{label}: max abs diff {max_diff} above {MAX_ABS_DIFF}"
            );

            eprintln!("{label}: OK (cos={cos:.6}, max_diff={max_diff:.8e})");
        }

        // short_en: query role.
        let seq = adapter
            .embed("How to configure Fjall persistence mode?", Role::Query)
            .unwrap();
        check_vector(
            &seq.vector,
            &fixture["cases"]["short_en"]["vector"],
            "short_en/query",
        );

        // Query vs passage asymmetry on same text.
        let q_seq = adapter
            .embed("How to configure Fjall persistence mode?", Role::Query)
            .unwrap();
        let p_seq = adapter
            .embed("How to configure Fjall persistence mode?", Role::Passage)
            .unwrap();
        check_vector(
            &p_seq.vector,
            &fixture["prefix"]["passage_vector"],
            "short_en/passage",
        );

        // The two roles must produce meaningfully different vectors.
        let cos_q_p: f32 = q_seq
            .vector
            .iter()
            .zip(p_seq.vector.iter())
            .map(|(a, b)| a * b)
            .sum();
        assert!(
            cos_q_p < 0.995,
            "query and passage embeddings should differ (cos={cos_q_p})"
        );

        // mixed_batch: padded batch of two different-length inputs (passage role).
        let short = "short one";
        let long = "a somewhat longer sentence about Rust memory models and ownership rules in the compiler";
        let results = adapter
            .embed_batch(&[
                EmbedInput {
                    text: short.into(),
                    role: Role::Passage,
                },
                EmbedInput {
                    text: long.into(),
                    role: Role::Passage,
                },
            ])
            .unwrap();
        assert_eq!(results.len(), 2);

        // Reference was generated with the same inputs (passage prefix included).
        check_vector(
            &results[0].vector,
            &fixture["cases"]["mixed_batch"]["vectors"][0],
            "mixed_batch[0]",
        );
        check_vector(
            &results[1].vector,
            &fixture["cases"]["mixed_batch"]["vectors"][1],
            "mixed_batch[1]",
        );

        // Batch row 0 must equal single embed (padding correctness).
        let r0 = adapter.embed(short, Role::Passage).unwrap();
        assert_eq!(
            results[0].vector, r0.vector,
            "batch row 0 must equal single embed"
        );

        // Padding correctness: batch attention masks reflect actual lengths.
        let len0 = results[0].attention_mask.iter().filter(|&&b| b).count();
        let len1 = results[1].attention_mask.iter().filter(|&&b| b).count();
        assert!(len1 > len0, "longer text must have more attended tokens");

        // empty_string case (passage role on empty string).
        let empty_seq = adapter.embed("", Role::Passage).unwrap();
        check_vector(
            &empty_seq.vector,
            &fixture["cases"]["empty_string"]["vector"],
            "empty_string",
        );

        // FR query (non-English).
        let fr_seq = adapter
            .embed("comment configurer la persistance ?", Role::Query)
            .unwrap();
        check_vector(
            &fr_seq.vector,
            &fixture["cases"]["fr_query"]["vector"],
            "fr_query",
        );

        // Code snippet.
        let code_seq = adapter
            .embed("fn main() { println!(\"hello\"); }", Role::Passage)
            .unwrap();
        check_vector(
            &code_seq.vector,
            &fixture["cases"]["code_snippet"]["vector"],
            "code_snippet",
        );
    }

    /// Projector bridge: the adapter exposes chunk_passage through the
    /// projector Embedder seam with rendered-coordinate spans, and its
    /// document embedding matches the Passage-role recipe exactly.
    #[test]
    fn projector_bridge_chunks_and_embeds_as_passage() {
        use crate::search::projector::Embedder as ProjectorEmbedder;
        let (cache, guard) = test_cache();
        if !populate_cache(&cache, &guard) {
            eprintln!("SKIP: artifacts not present");
            return;
        }
        let mut adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();
        let fixture = load_fixture();
        let title = fixture["long_document"]["title"].as_str().unwrap();
        let fragment = fixture["long_document"]["fragment"].as_str().unwrap();

        let units = adapter.chunk_text(title, fragment);
        let spans = adapter.chunk_passage(title, fragment);
        assert_eq!(units.len(), spans.len(), "one unit per derived chunk");
        assert!(units.len() > 1, "reference long document must chunk");
        let shift = title.len() as u64 + 1;
        for (u, s) in units.iter().zip(spans.iter()) {
            assert_eq!(u.text, format!("{title}\n{}", s.text));
            assert_eq!(
                (u.char_start, u.char_end),
                (shift + s.char_start as u64, shift + s.char_end as u64)
            );
        }

        // Document embedding through the seam equals the Passage-role recipe.
        // (Qualified syntax: the inherent role-taking `embed` shadows the
        // trait seam by name — the seam is the Passage-role projection.)
        let via_seam = ProjectorEmbedder::embed(&mut adapter, &units[0].text).unwrap();
        let via_recipe = adapter.embed(&units[0].text, Role::Passage).unwrap();
        assert_eq!(via_seam, via_recipe.vector);
    }

    /// Query bridge: the mutex-guarded adapter serves the query role through
    /// the search backend seam, with prefix-asymmetry evidence (Query !=
    /// Passage vectors for the same text).
    #[test]
    fn query_bridge_uses_query_role() {
        use crate::search::backend::QueryEmbedderProvider;
        use std::sync::Mutex;
        let (cache, guard) = test_cache();
        if !populate_cache(&cache, &guard) {
            eprintln!("SKIP: artifacts not present");
            return;
        }
        let mut adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();
        let text = "how to configure fjall persistence";
        let expected_q = adapter.embed(text, Role::Query).unwrap().vector;
        let expected_p = adapter.embed(text, Role::Passage).unwrap().vector;
        assert_ne!(
            expected_q, expected_p,
            "prefix asymmetry must hold for the bridge to be meaningful"
        );

        let bridged = Mutex::new(adapter);
        let via_bridge = bridged.embed_query(text).unwrap();
        assert_eq!(
            via_bridge, expected_q,
            "query bridge must embed with the Query role"
        );
    }

    /// Task 10: unsupported model recipes are rejected with actionable errors.
    #[test]
    fn t_emb_rejects_unsupported_recipe() {
        let dir = tempfile::tempdir().unwrap();

        // Write a config that looks like BERT but has wrong architecture.
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
                "architectures": ["XLMRobertaModel"],
                "model_type": "xlm-roberta",
                "hidden_size": 768,
                "vocab_size": 250002
            }"#,
        )
        .unwrap();

        let recipe = e5_small_recipe();
        match E5SmallAdapter::validate_config_file(dir.path(), &recipe) {
            Err(ArtifactError::UnsupportedModel(id, reason)) => {
                assert_eq!(id, recipe.id);
                assert!(
                    reason.contains("BertModel"),
                    "error should mention expected architecture"
                );
            }
            other => panic!("should reject XLMRobertaModel architecture, got: {other:?}"),
        }

        // Correct architecture but wrong tokenizer class.
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
                "architectures": ["BertModel"],
                "model_type": "bert",
                "hidden_size": 384,
                "vocab_size": 250037
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("tokenizer_config.json"),
            r#"{
                "tokenizer_class": "BertTokenizer"
            }"#,
        )
        .unwrap();

        match E5SmallAdapter::validate_config_file(dir.path(), &recipe) {
            Err(ArtifactError::UnsupportedModel(_, reason)) => {
                assert!(
                    reason.contains("XLMRobertaTokenizer"),
                    "error should mention expected tokenizer"
                );
            }
            other => panic!("should reject BertTokenizer class, got: {other:?}"),
        }

        // Correct architecture and tokenizer: passes validation.
        std::fs::write(
            dir.path().join("tokenizer_config.json"),
            r#"{
                "tokenizer_class": "XLMRobertaTokenizer"
            }"#,
        )
        .unwrap();
        assert!(E5SmallAdapter::validate_config_file(dir.path(), &recipe).is_ok());
    }

    /// T-EMB-03: long-document chunking preserves recall and parent identity.
    #[test]
    fn t_emb_03_long_document_chunking() {
        let (cache, guard) = test_cache();
        if !populate_cache(&cache, &guard) {
            eprintln!("SKIP: artifacts not present");
            return;
        }
        let mut adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();
        let fixture = load_fixture();

        const COSINE_TOL: f32 = 0.999;

        fn check_vector(actual: &[f32], expected_json: &serde_json::Value, label: &str) {
            let expected: Vec<f32> = expected_json
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_f64())
                .map(|v| v as f32)
                .collect();
            assert_eq!(actual.len(), expected.len(), "{label}: dimension mismatch");
            assert!(
                actual.iter().all(|v| v.is_finite()),
                "{label}: non-finite values"
            );
            let cos: f32 = actual.iter().zip(expected.iter()).map(|(a, b)| a * b).sum();
            assert!(
                cos > COSINE_TOL,
                "{label}: cosine {cos} below tolerance {COSINE_TOL}"
            );
        }

        // The reference fixture was generated with the same recipe as production:
        // prefix = "passage: {title}\n", greedy unit packing under 512 tokens.
        let title = fixture["long_document"]["title"].as_str().unwrap();
        let fragment = fixture["long_document"]["fragment"].as_str().unwrap();

        // Task 6: the Rust chunker must reproduce the reference chunks exactly —
        // same text, same parent-identity offsets (char_start/char_end).
        let rust_chunks = adapter.chunk_passage(title, fragment);
        let ref_chunks = fixture["long_document"]["chunks"].as_array().unwrap();
        assert_eq!(rust_chunks.len(), ref_chunks.len(), "chunk count mismatch");

        for (i, (rc, rf)) in rust_chunks.iter().zip(ref_chunks.iter()).enumerate() {
            assert_eq!(rc.text, rf["text"], "chunk[{i}] text mismatch");
            assert_eq!(
                rc.char_start as u64,
                rf["char_start"].as_u64().unwrap(),
                "chunk[{i}] char_start"
            );
            assert_eq!(
                rc.char_end as u64,
                rf["char_end"].as_u64().unwrap(),
                "chunk[{i}] char_end"
            );
            // Token count includes the bounded prefix.
            let ref_ids: Vec<u32> = rf["input_ids"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v as u32)
                .collect();
            assert_eq!(rc.token_count, ref_ids.len(), "chunk[{i}] token count");

            // Offsets must point at the exact span of this chunk in the fragment.
            let slice = &fragment[rc.char_start..rc.char_end];
            assert_eq!(slice, rc.text, "chunk[{i}] offsets do not match its text");

            // Embedding equivalence: feed the same prefixed input as the reference.
            let full_input = format!("{title}\n{}", rc.text);
            let seq = adapter.embed(&full_input, Role::Passage).unwrap();
            assert_eq!(seq.input_ids, ref_ids, "chunk[{i}] input_ids mismatch");
            check_vector(&seq.vector, &rf["vector"], &format!("long_doc/chunk[{i}]"));
        }

        // The fact is beyond the first model window: it must live in chunk 1.
        let n_chunks = ref_chunks.len();
        assert!(n_chunks >= 2, "fact must be beyond the first window");
        let last_chunk_text = rust_chunks.last().unwrap().text.clone();
        assert!(last_chunk_text.contains("ZEBRA-COPPER-HAMMOCK-7741"));

        // Tail-of-memory retrieval: query for the secret; best match is chunk 1.
        let q_seq = adapter
            .embed("ZEBRA-COPPER-HAMMOCK-7741", Role::Query)
            .unwrap();
        check_vector(
            &q_seq.vector,
            &fixture["cases"]["query_long_fact"]["vector"],
            "query_long_fact",
        );

        let mut best_idx = 0;
        let mut best_cos = f32::MIN;
        for (i, rf) in ref_chunks.iter().enumerate() {
            let expected: Vec<f32> = rf["vector"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_f64())
                .map(|v| v as f32)
                .collect();
            let cos: f32 = q_seq
                .vector
                .iter()
                .zip(expected.iter())
                .map(|(a, b)| a * b)
                .sum();
            if cos > best_cos {
                best_cos = cos;
                best_idx = i;
            }
        }

        assert!(
            best_idx == n_chunks - 1,
            "query should retrieve last chunk (fact location), got {best_idx}, best_cos={best_cos}"
        );
        eprintln!("T-EMB-03: OK — fact in chunk[{best_idx}] of {n_chunks}, cos={best_cos:.4}");
    }

    /// Task 6 unit checks: offsets, token bounds and determinism (no weights needed).
    #[test]
    fn t_emb_03_chunking_offsets_and_bounds() {
        let (cache, guard) = test_cache();
        if !populate_cache(&cache, &guard) {
            eprintln!("SKIP: artifacts not present");
            return;
        }
        let mut adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();

        // Short passage fits in one chunk covering the whole fragment.
        let frag = "alpha\nbeta\ngamma";
        let chunks = adapter.chunk_passage("T", frag);
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].char_start, chunks[0].char_end), (0, frag.len()));

        // Every chunk of a long passage fits the model window when prefixed.
        let unit =
            "Rust ownership rules and borrow checker behavior under concurrent access patterns. ";
        let frag_long = (0..40).map(|_| unit.trim()).collect::<Vec<_>>().join("\n");
        for c in adapter.chunk_passage("Long", &frag_long) {
            assert!(
                c.token_count <= 512,
                "chunk exceeds model window: {}",
                c.token_count
            );
        }

        // Determinism: same input -> identical chunks.
        let a = adapter.chunk_passage("Long", &frag_long);
        let b = adapter.chunk_passage("Long", &frag_long);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(
                (x.text.as_str(), x.char_start, x.char_end),
                (y.text.as_str(), y.char_start, y.char_end)
            );
        }

        // Coverage: chunks tile the fragment without gaps or overlaps.
        let mut prev_end = 0usize;
        for c in &a {
            assert!(c.char_start >= prev_end, "chunks must not overlap");
            prev_end = c.char_end.max(prev_end);
        }

        // Oversized single unit (one line > 512 tokens) is hard-split at the
        // tokenizer limit: every resulting chunk fits the window when prefixed.
        let word = "token";
        let mut n_words = 1usize;
        loop {
            let t = vec![word; n_words].join(" ");
            if adapter.raw_tokenize(&format!("passage: {t}")).len() > 560 {
                break;
            }
            n_words += 1;
        }
        let huge_line = vec![word; n_words].join(" ");
        let chunks = adapter.chunk_passage("T", &huge_line);
        assert!(
            chunks.len() >= 2,
            "oversized line must be split: {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                c.token_count <= 512,
                "hard-split chunk exceeds window: {}",
                c.token_count
            );
            // Verbatim spans of the original fragment.
            assert_eq!(&huge_line[c.char_start..c.char_end], c.text);
        }

        // near-limit case (T-EMB-02): an input at/just under the 512-token model
        // window must embed correctly — finite, unit-normalized, no silent truncation.
        let word = "token";
        let mut n_words = 1usize;
        loop {
            let t = vec![word; n_words].join(" ");
            if adapter.raw_tokenize(&format!("passage: {t}")).len() >= 512 {
                break;
            }
            n_words += 1;
        }
        // Trim whole words until we are at or under the window.
        let mut near_text = vec![word; n_words].join(" ");
        while adapter.raw_tokenize(&format!("passage: {near_text}")).len() > 512 {
            match near_text.rfind(' ') {
                Some(pos) => near_text.truncate(pos),
                None => break,
            }
        }
        let near_tokens = adapter.raw_tokenize(&format!("passage: {near_text}")).len();
        assert!(
            (508..=512).contains(&near_tokens),
            "expected a near-limit input, got {near_tokens} tokens"
        );

        let seq = adapter.embed(&near_text, Role::Passage).unwrap();
        assert_eq!(seq.input_ids.len(), near_tokens);
        assert!(
            seq.vector.iter().all(|v| v.is_finite()),
            "non-finite at window limit"
        );
        let norm_sq: f32 = seq.vector.iter().map(|v| v * v).sum();
        assert!(
            (norm_sq - 1.0).abs() < 1e-4,
            "not unit-normalized at limit (norm^2={norm_sq})"
        );

        // Over-limit input is rejected with an actionable error (no silent truncation).
        let too_long = format!("{near_text} extra word here to push it over the window");
        assert!(adapter.embed(&too_long, Role::Passage).is_err());
    }
}
