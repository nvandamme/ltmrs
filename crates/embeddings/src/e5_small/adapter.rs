//! E5-small adapter implementation (moved verbatim from `e5_small.rs`).

use std::path::Path;
use std::sync::Arc;

use super::{Chunk, E5SmallAdapter, EmbedInput, EmbeddedSequence};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use tokenizers::{Encoding, Tokenizer};

use crate::artifacts::{ArtifactCache, ArtifactError, ArtifactResult, HttpClient};
use crate::bert_impl::{BertConfig, BertModel};
use crate::manifest::{ModelRecipe, e5_small_artifact, e5_small_recipe};
use crate::recipe::{Normalization, Role};

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

    /// Explicit fetch + verify, then load (online mode; `--provision-models`).
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
    pub(crate) fn raw_tokenize(&self, text: &str) -> Vec<u32> {
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
                    // A prefix that alone fills or overflows the window discloses
                    // as-is (checked_sub: usize underflow would panic in debug,
                    // wrap to a huge budget in release).
                    let Some(budget) = max_tokens
                        .checked_sub(self.raw_tokenize(&prefix).len())
                        .filter(|b| *b > 0)
                    else {
                        // Pathological prefix fills the window; disclose as-is.
                        chunks.push(Chunk {
                            text: alone.to_string(),
                            char_start: *s,
                            char_end: *e,
                            token_count: 0, // filled below after prefixing
                        });
                        continue;
                    };
                    // The filter above guarantees budget > 0: the budget-0
                    // case is handled by the disclose-as-is branch.
                    {
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
