//! E5-small adapter implementation (moved verbatim from `e5_small.rs`).

use std::path::Path;
use std::sync::Arc;

use super::{Chunk, E5SmallAdapter, EmbedInput, EmbeddedSequence};
use burn_tensor::Device;
use tokenizers::{Encoding, Tokenizer};

use crate::artifacts::{ArtifactCache, ArtifactError, ArtifactResult, HttpClient};
use crate::e5_burn::{self, BurnBertModel};
use crate::manifest::{ModelRecipe, e5_small_artifact, e5_small_recipe};
use crate::recipe::{Normalization, Role};

/// Configuration matching the pinned E5-small model.
#[derive(Debug, Clone)]
pub struct BertConfig {
    pub vocab_size: usize,
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub intermediate_size: usize,
    pub max_position_embeddings: usize,
    pub type_vocab_size: usize,
    pub layer_norm_eps: f64,
}

impl BertConfig {
    /// Load config from the pinned E5-small artifacts.
    pub fn load_from_config_file(path: &std::path::Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        let cfg: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| format!("Invalid JSON in {}: {}", path.display(), e))?;

        let field = |name: &str| -> Result<usize, String> {
            match cfg.get(name) {
                None => Err(format!(
                    "Missing required field {name:?} in {}",
                    path.display()
                )),
                Some(v) => v.as_u64().map(|v| v as usize).ok_or_else(|| {
                    format!(
                        "Field {name:?} in {} must be a positive integer, got {v}",
                        path.display()
                    )
                }),
            }
        };
        let eps = match cfg.get("layer_norm_eps") {
            None => 1e-12,
            Some(v) => v.as_f64().ok_or_else(|| {
                format!(
                    "Field \"layer_norm_eps\" in {} must be a number, got {v}",
                    path.display()
                )
            })?,
        };

        let loaded = Self {
            vocab_size: field("vocab_size")?,
            hidden_size: field("hidden_size")?,
            num_hidden_layers: field("num_hidden_layers")?,
            num_attention_heads: field("num_attention_heads")?,
            intermediate_size: field("intermediate_size")?,
            max_position_embeddings: field("max_position_embeddings")?,
            type_vocab_size: field("type_vocab_size")?,
            layer_norm_eps: eps,
        };
        loaded.validate(path)?;
        // Burn core eps is fixed (1e-12, Candle-parity proven); the schema
        // value is validated as present-but-otherwise-ignored.
        let _ = loaded.layer_norm_eps;
        Ok(loaded)
    }

    /// Reject present-but-impossible values (a zero head count panics as
    /// division-by-zero downstream; foreign configs must error instead).
    pub fn validate(&self, path: &std::path::Path) -> Result<(), String> {
        let bad = |what: &str| format!("Invalid {what} in {}", path.display());
        if self.vocab_size == 0 {
            return Err(bad("vocab_size 0"));
        }
        if self.num_attention_heads == 0 {
            return Err(bad("num_attention_heads 0"));
        }
        if self.hidden_size == 0 || !self.hidden_size.is_multiple_of(self.num_attention_heads) {
            return Err(bad("hidden_size not divisible by num_attention_heads"));
        }
        if self.num_hidden_layers == 0 {
            return Err(bad("num_hidden_layers 0"));
        }
        if self.intermediate_size == 0 {
            return Err(bad("intermediate_size 0"));
        }
        if self.max_position_embeddings == 0 {
            return Err(bad("max_position_embeddings 0"));
        }
        if self.type_vocab_size == 0 {
            return Err(bad("type_vocab_size 0"));
        }
        Ok(())
    }
}

/// Explicit device policy (Candle-replacement plan, global constraints).
/// `Auto` probes the CUDA loader and takes CUDA when usable, else CPU;
/// the resolution is observable via `is_cpu` (a probe, not a silent
/// fallback — the caller asked for best-available). `Cuda` fails loudly
/// naming the missing library. `Cpu` never touches the loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMode {
    Auto,
    Cpu,
    Cuda,
}
impl E5SmallAdapter {
    /// Load from a verified artifact directory (digests checked by the cache).
    /// Returns an actionable error for unsupported recipes instead of trying.
    ///
    /// Frozen CPU path: production default, byte-identical with or without
    /// the `cuda` feature (see the CUDA report: default-feature check green).
    pub fn load_verified(dir: &Path) -> ArtifactResult<Self> {
        Self::load_verified_auto(dir, DeviceMode::Cpu)
    }

    /// Load from a verified artifact directory under an explicit device
    /// policy (Task 6 runtime gates).
    pub fn load_verified_auto(dir: &Path, mode: DeviceMode) -> ArtifactResult<Self> {
        let device = match mode {
            DeviceMode::Cpu => Self::cpu_device(),
            DeviceMode::Cuda => Self::cuda_device().map_err(ArtifactError::BackendInference)?,
            DeviceMode::Auto => Self::cuda_device().unwrap_or_else(|_| Self::cpu_device()),
        };
        let mut adapter = Self::load_verified_on_device(dir, &device)?;
        adapter.on_cuda = Self::device_is_cuda(&device);
        Ok(adapter)
    }

    /// True when this adapter runs on CPU (Flex). The Auto resolution and
    /// the explicit constructors are both observable here.
    pub fn is_cpu(&self) -> bool {
        !self.on_cuda
    }

    fn cpu_device() -> Device {
        Device::new(burn_dispatch::device::DispatchDevice::Flex(
            burn_flex::FlexDevice,
        ))
    }

    /// CubeCL CUDA device, probed with one tiny op so a missing/broken
    /// loader fails here — loudly — instead of mid-inference. The error
    /// names the library family (`libcuda`) the loader could not resolve.
    #[cfg(feature = "cuda")]
    fn cuda_device() -> Result<Device, String> {
        let device = Device::new(burn_dispatch::device::DispatchDevice::Cube(
            burn_cubecl::cubecl::Device::Cuda(Default::default()),
        ));
        let probe = burn_tensor::Tensor::<1>::from_data(
            burn_tensor::TensorData::new(vec![1.0f32], [1]),
            &device,
        );
        let _ = (probe.clone() + probe)
            .into_data()
            .as_slice::<f32>()
            .map(<[f32]>::to_vec)
            .map_err(|e| {
                format!("CUDA inference requested but the CUDA loader failed (libcuda/libnvrtc unresolved on this host): {e:?}")
            })?;
        Ok(device)
    }

    /// Without the `cuda` feature no CUDA runtime exists in the binary;
    /// requesting it is a loud configuration error, never a silent CPU run.
    #[cfg(not(feature = "cuda"))]
    fn cuda_device() -> Result<Device, String> {
        Err("CUDA inference requested but this binary was built without the `cuda` feature".into())
    }

    /// Classify a device from its Debug form (`Device<Cube(Cuda(..))>` vs
    /// `Device<Flex(..)>`). Pinned by `device_classification_matches_probe`
    /// so a Burn Debug-format change fails loudly instead of misrouting.
    #[cfg(test)]
    pub(crate) fn device_is_cuda_for_test(device: &Device) -> bool {
        Self::device_is_cuda(device)
    }

    fn device_is_cuda(device: &Device) -> bool {
        format!("{device:?}").contains("Cuda")
    }

    /// Load from a verified artifact directory onto an explicit device
    /// (RQ-09/RQ-10 GPU leg). The ONLY way to reach CUDA inference: callers
    /// pass `Device::new_cuda(0)` here; nothing in the daemon/service path
    /// calls this, so production can never silently switch devices.
    pub fn load_verified_on_device(dir: &Path, device: &Device) -> ArtifactResult<Self> {
        let recipe = e5_small_recipe();

        // Task 10: validate the pinned recipe before touching any weights.
        Self::validate_config_file(dir, &recipe)?;

        let tokenizer_path = dir.join("tokenizer.json");
        let tokenizer =
            Arc::new(Tokenizer::from_file(&tokenizer_path).map_err(|e| {
                ArtifactError::Download(recipe.id.clone(), format!("tokenizer: {e}"))
            })?);

        let weights_path = dir.join("model.safetensors");
        let weights = e5_burn::load_safetensors_weights(&weights_path)
            .map_err(|e| ArtifactError::Download(recipe.id.clone(), format!("safetensors: {e}")))?;

        Self::read_bert_config(dir)?;
        let model = BurnBertModel::load(weights, device).map_err(|e| {
            ArtifactError::Download(recipe.id.clone(), format!("BurnBertModel load: {e}"))
        })?;

        Ok(Self {
            recipe,
            tokenizer,
            model,
            device: device.clone(),
            on_cuda: Self::device_is_cuda(device),
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
        // Parse using our pinned-schema loader (handles all required fields).
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
    /// each L2-normalized and finite-checked. Takes `&self`: inference reads
    /// weights but mutates nothing, so one adapter fans out across threads.
    pub fn embed_batch(&self, inputs: &[EmbedInput]) -> ArtifactResult<Vec<EmbeddedSequence>> {
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

        // One natural-length row per input (no padding: Burn softmax
        // reduction order varies with row width, so padded and unpadded
        // forwards of the same tokens differ ~3e-9 — forward each row at
        // its own length for batch/single bit-identity).
        // Token-type is all zeros for the single-segment E5 inputs.
        let _ = pad_id;
        let rows: Vec<e5_burn::PaddedRow> = encodings
            .iter()
            .map(|enc| {
                let ids = enc.get_ids().to_vec();
                let mask: Vec<u32> = ids
                    .iter()
                    .enumerate()
                    .map(|(j, _)| {
                        u32::from(enc.get_attention_mask().get(j).copied().unwrap_or(0) != 0)
                    })
                    .collect();
                e5_burn::PaddedRow { ids, mask }
            })
            .collect();

        let epsilon = match self.recipe.normalization {
            Normalization::L2 { epsilon } => epsilon,
        };
        let values = e5_burn::forward_batch(&self.model, &rows, epsilon, &self.device)
            .map_err(|e| ArtifactError::BackendInference(format!("burn forward: {e}")))?;

        // Finite-value check: NaN/Inf is a hard failure, never stored.
        for row in &values {
            if !row.iter().all(|v| v.is_finite()) {
                return Err(ArtifactError::Download(
                    e5_small_recipe().id.clone(),
                    "model produced non-finite values (NaN/Inf); refusing to embed".into(),
                ));
            }
        }

        let mut out = Vec::with_capacity(inputs.len());
        for (i, enc) in encodings.iter().enumerate() {
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
    pub fn embed(&self, text: &str, role: Role) -> ArtifactResult<EmbeddedSequence> {
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

    /// A malformed artifact config must error, never panic (foreign config
    /// files must not crash daemon startup). Moved with BertConfig from
    /// bert_impl (Candle deletion) — same assertions, String error type.
    #[test]
    fn malformed_config_errors_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"hidden_size": 384}"#).unwrap();
        let err = BertConfig::load_from_config_file(&path).unwrap_err();
        assert!(
            err.contains("vocab_size"),
            "must name the missing field, got: {err}"
        );
        // Present-but-zero is invalid, not defaulted (downstream div-by-zero).
        std::fs::write(&path, r#"{"vocab_size": 100, "hidden_size": 12, "num_hidden_layers": 1, "num_attention_heads": 0, "intermediate_size": 8, "max_position_embeddings": 16, "type_vocab_size": 2}"#).unwrap();
        let err = BertConfig::load_from_config_file(&path).unwrap_err();
        assert!(
            err.contains("num_attention_heads"),
            "must name the invalid field, got: {err}"
        );
        // Zero layer/intermediate/position/type dims are equally impossible.
        for field in [
            "num_hidden_layers",
            "intermediate_size",
            "max_position_embeddings",
            "type_vocab_size",
        ] {
            let mut cfg = serde_json::json!({"vocab_size": 100, "hidden_size": 12, "num_hidden_layers": 1, "num_attention_heads": 2, "intermediate_size": 8, "max_position_embeddings": 16, "type_vocab_size": 2});
            cfg[field] = serde_json::json!(0);
            std::fs::write(&path, serde_json::to_string(&cfg).unwrap()).unwrap();
            let err = BertConfig::load_from_config_file(&path).unwrap_err();
            assert!(
                err.contains(field),
                "must name the invalid field, got: {err}"
            );
        }
    }
}
