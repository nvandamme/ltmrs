//! Burn-based E5-small tensor core (Candle replacement, Task 4).
//!
//! Same recipe, same weights file, same public shapes as the Candle
//! `bert_impl` core it replaces: XLM-RoBERTa tokenizer (unchanged,
//! outside this module), BertModel backbone, query/passage prefixes,
//! attention-mask-aware mean pooling and L2 normalization (pooling lives
//! in the adapter, unchanged). Only the tensor execution moves.
//!
//! Device policy: CPU (Flex backend) by default; CUDA via CubeCL device
//! only when explicitly requested AND the runtime loader reports usable
//! hardware. No env-var switching, no silent fallback.

use burn_tensor::Device;

/// Load a safetensors weight file into a name -> flat-f32 map (same file
/// the Candle path reads; format knowledge stays here, not in the adapter).
pub fn load_safetensors_weights(
    path: &std::path::Path,
) -> Result<std::collections::BTreeMap<String, Vec<f32>>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read weights: {e}"))?;
    let safetensors = safetensors::SafeTensors::deserialize(&bytes)
        .map_err(|e| format!("parse safetensors: {e}"))?;
    let mut out = std::collections::BTreeMap::new();
    for name in safetensors
        .names()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
    {
        let view = safetensors
            .tensor(&name)
            .map_err(|e| format!("tensor {name}: {e}"))?;
        let f32data: Vec<f32> = match view.dtype() {
            safetensors::Dtype::F32 => {
                let (chunks, _) = view.data().as_chunks::<4>();
                chunks.iter().map(|b| f32::from_le_bytes(*b)).collect()
            }
            safetensors::Dtype::F16 => {
                let (chunks, _) = view.data().as_chunks::<2>();
                chunks
                    .iter()
                    .map(|b| half::f16::from_le_bytes(*b).to_f32())
                    .collect()
            }
            safetensors::Dtype::BF16 => {
                // Manual bf16 -> f32: exponent rebias.
                let (chunks, _) = view.data().as_chunks::<2>();
                chunks
                    .iter()
                    .map(|b| {
                        let bits = u16::from_le_bytes(*b) as u32;
                        f32::from_bits(bits << 16)
                    })
                    .collect()
            }
            // Index buffers (e.g. position_ids) are runtime-generated,
            // not weights: skip integer dtypes instead of failing.
            safetensors::Dtype::I64
            | safetensors::Dtype::I32
            | safetensors::Dtype::U8
            | safetensors::Dtype::BOOL => continue,
            other => return Err(format!("unsupported dtype {other:?} for {name}")),
        };
        out.insert(name, f32data);
    }
    Ok(out)
}

/// Frozen-case entry point for the parity test: embed one fixture case
/// with the Burn backend. GREEN: full Burn BERT forward on Flex CPU.
pub fn embed_frozen_case(case: &str) -> Result<Vec<f32>, String> {
    // Fixture geometry (matches reference_fixture.json + recipe):
    // multilingual-e5-small, 12 layers, hidden 384, 12 heads, vocab 250037.
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tmp/e5-artifacts");
    let weights = load_safetensors_weights(&dir.join("model.safetensors"))?;
    let device = Device::new(burn_dispatch::device::DispatchDevice::Flex(
        burn_flex::FlexDevice,
    ));
    let model = BurnBertModel::load(weights, &device)?;
    // Fixture tokenizer outputs (from reference_fixture.json; the
    // tokenizer itself is unchanged and lives outside this module).
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/reference_fixture.json"))
            .map_err(|e| e.to_string())?;
    let c = fixture["cases"][case]
        .as_object()
        .ok_or_else(|| format!("unknown fixture case {case:?}"))?;
    // Batch cases carry rows; single cases carry one stream.
    let rows: Vec<(Vec<u32>, Vec<u32>)> = if let Some(texts) = c.get("texts") {
        let _ = texts;
        let id_rows: Vec<Vec<u32>> =
            serde_json::from_value(c["input_ids"].clone()).map_err(|e| e.to_string())?;
        let mask_rows: Vec<Vec<u32>> =
            serde_json::from_value(c["attention_mask"].clone()).map_err(|e| e.to_string())?;
        id_rows.into_iter().zip(mask_rows).collect()
    } else {
        let ids: Vec<u32> =
            serde_json::from_value(c["input_ids"].clone()).map_err(|e| e.to_string())?;
        let mask: Vec<u32> =
            serde_json::from_value(c["attention_mask"].clone()).map_err(|e| e.to_string())?;
        vec![(ids, mask)]
    };
    // For batch cases the caller selects the row; default row 0.
    let (ids, mask) = &rows[0];
    let device = Device::new(burn_dispatch::device::DispatchDevice::Flex(
        burn_flex::FlexDevice,
    ));
    let hidden = model.forward(ids, mask, &device)?;
    // Attention-mask-aware mean pooling + L2 normalization (same recipe
    // as the Candle adapter: mean over unmasked positions, then unit norm).
    Ok(pool_and_norm(&hidden, ids.len(), mask, 1e-12))
}

/// Batched inference for the adapter: one padded row per input (pad ids
/// with mask 0), forward + mask-mean + L2 on the caller's device, one
/// normalized vector per row. Empty rows are a caller bug — fail loud.
pub fn forward_batch(
    model: &BurnBertModel,
    rows: &[PaddedRow],
    epsilon: f32,
    device: &Device,
) -> Result<Vec<Vec<f32>>, String> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if row.ids.is_empty() {
            return Err("empty input row".to_string());
        }
        let hidden = model.forward(&row.ids, &row.mask, device)?;
        out.push(pool_and_norm(&hidden, row.ids.len(), &row.mask, epsilon));
    }
    Ok(out)
}

/// One padded input row: ids padded to the batch max length with pad ids,
/// mask 0 on pads (excluded from the mean).
pub struct PaddedRow {
    pub ids: Vec<u32>,
    pub mask: Vec<u32>,
}

/// Attention-mask-aware mean pooling + L2 normalization (same recipe as
/// the Burn port: mean over unmasked positions, then unit norm).
fn pool_and_norm(hidden: &[f32], seq_len: usize, mask: &[u32], epsilon: f32) -> Vec<f32> {
    let dim = hidden.len() / seq_len.max(1);
    let mut pooled = vec![0.0f32; dim];
    let mut count = 0u32;
    for i in 0..seq_len {
        if mask[i] != 0 {
            for h in 0..dim {
                pooled[h] += hidden[i * dim + h];
            }
            count += 1;
        }
    }
    let count = count.max(1) as f32;
    for x in pooled.iter_mut() {
        *x /= count;
    }
    let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
    let inv = norm.max(epsilon).recip();
    for x in pooled.iter_mut() {
        *x *= inv;
    }
    pooled
}

/// Row variant for batch fixture cases (e.g. mixed_batch rows).
pub fn embed_frozen_case_row(case: &str, row: usize) -> Result<Vec<f32>, String> {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tmp/e5-artifacts");
    let weights = load_safetensors_weights(&dir.join("model.safetensors"))?;
    let device = Device::new(burn_dispatch::device::DispatchDevice::Flex(
        burn_flex::FlexDevice,
    ));
    let model = BurnBertModel::load(weights, &device)?;
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/reference_fixture.json"))
            .map_err(|e| e.to_string())?;
    let c = fixture["cases"][case]
        .as_object()
        .ok_or_else(|| format!("unknown fixture case {case:?}"))?;
    let id_rows: Vec<Vec<u32>> =
        serde_json::from_value(c["input_ids"].clone()).map_err(|e| e.to_string())?;
    let mask_rows: Vec<Vec<u32>> =
        serde_json::from_value(c["attention_mask"].clone()).map_err(|e| e.to_string())?;
    let (ids, mask) = (&id_rows[row], &mask_rows[row]);
    let device = Device::new(burn_dispatch::device::DispatchDevice::Flex(
        burn_flex::FlexDevice,
    ));
    let hidden = model.forward(ids, mask, &device)?;
    Ok(pool_and_norm(&hidden, ids.len(), mask, 1e-12))
}

/// Burn-native BERT encoder: mirrors `bert_impl::BertModel` block for
/// block (embeddings -> N encoder layers) on Burn tensors, weights
/// injected from the same safetensors file. No Candle types cross this
/// boundary. Device comes from the caller (Flex CPU default; CubeCL CUDA
/// only when explicitly requested with usable hardware).
use burn_core::module::{Param, ParamId};
use burn_nn::{Embedding, LayerNorm, LayerNormConfig, Linear};
use burn_tensor::{Int, Tensor, TensorData};

pub struct BurnBertModel {
    word_emb: Embedding,
    pos_emb: Embedding,
    tok_emb: Embedding,
    emb_ln: LayerNorm,
    layers: Vec<BurnBertLayer>,
}

pub struct BurnBertLayer {
    q: Linear,
    k: Linear,
    v: Linear,
    o: Linear,
    o_ln: LayerNorm,
    inter: Linear,
    d: Linear,
    d_ln: LayerNorm,
}

fn take(
    weights: &std::collections::BTreeMap<String, Vec<f32>>,
    name: &str,
) -> Result<Vec<f32>, String> {
    weights
        .get(name)
        .cloned()
        .ok_or_else(|| format!("missing weight {name}"))
}

fn tensor2(rows: Vec<f32>, d0: usize, d1: usize, device: &Device) -> Tensor<2> {
    Tensor::from_data(TensorData::new(rows, [d0, d1]), device)
}

fn tensor1(row: Vec<f32>, d0: usize, device: &Device) -> Tensor<1> {
    Tensor::from_data(TensorData::new(row, [d0]), device)
}

fn linear_param(
    weight_flat_out_major: Vec<f32>,
    bias_flat: Vec<f32>,
    in_dim: usize,
    out_dim: usize,
    device: &Device,
) -> Linear {
    // Candle Linear stores weight as [out, in] (flat port reads
    // w[j * k + l]); Burn Row-major Linear expects [in, out].
    assert_eq!(weight_flat_out_major.len(), out_dim * in_dim);
    let mut row_major = vec![0.0f32; in_dim * out_dim];
    for o in 0..out_dim {
        for i in 0..in_dim {
            row_major[i * out_dim + o] = weight_flat_out_major[o * in_dim + i];
        }
    }
    Linear {
        weight: Param::initialized(ParamId::new(), tensor2(row_major, in_dim, out_dim, device)),
        bias: Some(Param::initialized(
            ParamId::new(),
            tensor1(bias_flat, out_dim, device),
        )),
    }
}

fn embedding_param(
    rows: Vec<f32>,
    n_embedding: usize,
    d_model: usize,
    device: &Device,
) -> Embedding {
    assert_eq!(rows.len(), n_embedding * d_model);
    Embedding {
        weight: Param::initialized(ParamId::new(), tensor2(rows, n_embedding, d_model, device)),
    }
}

fn layernorm_param(
    w: Vec<f32>,
    b: Vec<f32>,
    d_model: usize,
    eps: f64,
    device: &Device,
) -> LayerNorm {
    assert_eq!(w.len(), d_model);
    assert_eq!(b.len(), d_model);
    let mut ln: LayerNorm = LayerNormConfig::new(d_model).with_epsilon(eps).init(device);
    ln.gamma = Param::initialized(ParamId::new(), tensor1(w, d_model, device));
    ln.beta = Some(Param::initialized(
        ParamId::new(),
        tensor1(b, d_model, device),
    ));
    ln
}

impl BurnBertModel {
    /// Load from the injected weight map. E5-small geometry is fixed
    /// (12 layers, hidden 384, 12 heads, vocab 250037, intermediate 1536);
    /// the config file check stays in the adapter (unchanged validation).
    pub fn load(
        weights: std::collections::BTreeMap<String, Vec<f32>>,
        device: &Device,
    ) -> Result<Self, String> {
        let hidden = 384usize;
        let vocab = 250037usize;
        let intermediate = 1536usize;
        let n_layers = 12usize;
        let eps = 1e-12;
        let mut layers = Vec::with_capacity(n_layers);
        for i in 0..n_layers {
            let p = format!("encoder.layer.{i}");
            let linear = |suf: &str, in_dim: usize, out_dim: usize| -> Result<Linear, String> {
                Ok(linear_param(
                    take(&weights, &format!("{p}.{suf}.weight"))?,
                    take(&weights, &format!("{p}.{suf}.bias"))?,
                    in_dim,
                    out_dim,
                    device,
                ))
            };
            let ln = |suf: &str| -> Result<LayerNorm, String> {
                Ok(layernorm_param(
                    take(&weights, &format!("{p}.{suf}.weight"))?,
                    take(&weights, &format!("{p}.{suf}.bias"))?,
                    hidden,
                    eps,
                    device,
                ))
            };
            layers.push(BurnBertLayer {
                q: linear("attention.self.query", hidden, hidden)?,
                k: linear("attention.self.key", hidden, hidden)?,
                v: linear("attention.self.value", hidden, hidden)?,
                o: linear("attention.output.dense", hidden, hidden)?,
                o_ln: ln("attention.output.LayerNorm")?,
                inter: linear("intermediate.dense", hidden, intermediate)?,
                d: linear("output.dense", intermediate, hidden)?,
                d_ln: ln("output.LayerNorm")?,
            });
        }
        Ok(Self {
            word_emb: embedding_param(
                take(&weights, "embeddings.word_embeddings.weight")?,
                vocab,
                hidden,
                device,
            ),
            pos_emb: embedding_param(
                take(&weights, "embeddings.position_embeddings.weight")?,
                512,
                hidden,
                device,
            ),
            tok_emb: embedding_param(
                take(&weights, "embeddings.token_type_embeddings.weight")?,
                2,
                hidden,
                device,
            ),
            emb_ln: layernorm_param(
                take(&weights, "embeddings.LayerNorm.weight")?,
                take(&weights, "embeddings.LayerNorm.bias")?,
                hidden,
                eps,
                device,
            ),
            layers,
        })
    }

    /// Full forward: input ids + attention mask -> [seq_len, hidden]
    /// last-hidden-states (flat row-major). Mirrors `BertModel::forward`
    /// exactly on Burn tensors (embeddings, per-layer attention + MLP,
    /// no pooler).
    pub fn forward(&self, ids: &[u32], mask: &[u32], device: &Device) -> Result<Vec<f32>, String> {
        let seq_len = ids.len();
        if seq_len == 0 {
            return Err("empty input".to_string());
        }
        let input: Tensor<2, Int> =
            Tensor::from_data(TensorData::new(ids.to_vec(), [1, seq_len]), device);
        let word = self.word_emb.forward(input.clone());
        let pos_ids: Vec<u32> = (0..seq_len as u32).collect();
        let pos_input: Tensor<2, Int> =
            Tensor::from_data(TensorData::new(pos_ids, [1, seq_len]), device);
        let pos = self.pos_emb.forward(pos_input);
        let tok_ids = vec![0u32; seq_len];
        let tok_input: Tensor<2, Int> =
            Tensor::from_data(TensorData::new(tok_ids, [1, seq_len]), device);
        let tok = self.tok_emb.forward(tok_input);
        let mut hidden: Tensor<3> = self.emb_ln.forward(word + pos + tok);
        for layer in &self.layers {
            hidden = layer.forward(hidden, mask, seq_len, device)?;
        }
        let data = hidden.into_data();
        data.as_slice::<f32>()
            .map(<[f32]>::to_vec)
            .map_err(|e| format!("to vec: {e:?}"))
    }
}

impl BurnBertLayer {
    fn forward(
        &self,
        hidden: Tensor<3>,
        mask: &[u32],
        seq_len: usize,
        device: &Device,
    ) -> Result<Tensor<3>, String> {
        use burn_tensor::activation::{gelu, softmax};
        let h = 384;
        let heads = 12;
        let hd = h / heads;
        // Q/K/V projections: [1, S, H].
        let q = self.q.forward(hidden.clone());
        let k = self.k.forward(hidden.clone());
        let v = self.v.forward(hidden.clone());
        // Split heads: [1, S, H] -> [1, S, heads, hd] -> [1, heads, S, hd].
        let split = |t: Tensor<3>| t.reshape([1, seq_len, heads, hd]).swap_dims(1, 2);
        let (q, k, v) = (split(q), split(k), split(v));
        // Scores: [1, heads, S, S]; scale; additive mask bias.
        let scores = q.matmul(k.transpose());
        let scale = (hd as f32).sqrt().recip();
        let scores = scores * scale;
        let mut bias = vec![0.0f32; seq_len * seq_len];
        for j in 0..seq_len {
            if mask[j] == 0 {
                for i in 0..seq_len {
                    bias[i * seq_len + j] = -1e9f32;
                }
            }
        }
        let bias_t: Tensor<4> =
            Tensor::from_data(TensorData::new(bias, [1, 1, seq_len, seq_len]), device);
        let probs = softmax(scores + bias_t, 3);
        // Context: [1, heads, S, hd] -> [1, S, H].
        let ctx = probs.matmul(v).swap_dims(1, 2).reshape([1, seq_len, h]);
        // Output projection + residual + norm.
        let attn_out = self.o_ln.forward(self.o.forward(ctx) + hidden);
        // Intermediate (gelu-erf) + output + residual + norm.
        let inter = gelu(self.inter.forward(attn_out.clone()));
        let out = self.d_ln.forward(self.d.forward(inter) + attn_out);
        Ok(out)
    }
}
