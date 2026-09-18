//! Custom BERT implementation with proper batched input handling for E5-small.
//!
//! This avoids a bug in candle-transformers 0.11.0 where position embeddings
//! are not properly broadcast for batched inputs (creates [seq_len, hidden]
//! instead of [batch_size, seq_len, hidden]).

use candle_core::{Device, Tensor};
use candle_nn::Module;
use candle_nn::VarBuilder;

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
    pub fn load_from_config_file(path: &std::path::Path) -> Result<Self, candle_core::Error> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            candle_core::Error::Msg(format!("Failed to read {}: {}", path.display(), e))
        })?;
        let cfg: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            candle_core::Error::Msg(format!("Invalid JSON in {}: {}", path.display(), e))
        })?;

        Ok(Self {
            vocab_size: cfg["vocab_size"].as_u64().unwrap() as usize,
            hidden_size: cfg["hidden_size"].as_u64().unwrap() as usize,
            num_hidden_layers: cfg["num_hidden_layers"].as_u64().unwrap() as usize,
            num_attention_heads: cfg["num_attention_heads"].as_u64().unwrap() as usize,
            intermediate_size: cfg["intermediate_size"].as_u64().unwrap() as usize,
            max_position_embeddings: cfg["max_position_embeddings"].as_u64().unwrap() as usize,
            type_vocab_size: cfg["type_vocab_size"].as_u64().unwrap() as usize,
            layer_norm_eps: cfg
                .get("layer_norm_eps")
                .and_then(|v| v.as_f64())
                .unwrap_or(1e-12),
        })
    }

    fn attention_head_size(&self) -> usize {
        self.hidden_size / self.num_attention_heads
    }
}

/// A single transformer encoder layer.
struct BertLayer {
    attention_query: candle_nn::Linear,
    attention_key: candle_nn::Linear,
    attention_value: candle_nn::Linear,
    attention_output_dense: candle_nn::Linear,
    attention_output_layernorm: candle_nn::LayerNorm,

    intermediate_dense: candle_nn::Linear,
    output_dense: candle_nn::Linear,
    output_layernorm: candle_nn::LayerNorm,

    head_size: usize,
}

impl BertLayer {
    fn load(vb: VarBuilder, config: &BertConfig) -> Result<Self, candle_core::Error> {
        let all_head_size = config.hidden_size;

        Ok(Self {
            attention_query: candle_nn::linear(
                config.hidden_size,
                all_head_size,
                vb.pp("attention.self.query"),
            )?,
            attention_key: candle_nn::linear(
                config.hidden_size,
                all_head_size,
                vb.pp("attention.self.key"),
            )?,
            attention_value: candle_nn::linear(
                config.hidden_size,
                all_head_size,
                vb.pp("attention.self.value"),
            )?,
            attention_output_dense: candle_nn::linear(
                all_head_size,
                config.hidden_size,
                vb.pp("attention.output.dense"),
            )?,
            attention_output_layernorm: candle_nn::layer_norm(
                config.hidden_size,
                config.layer_norm_eps,
                vb.pp("attention.output.LayerNorm"),
            )?,

            intermediate_dense: candle_nn::linear(
                config.hidden_size,
                config.intermediate_size,
                vb.pp("intermediate.dense"),
            )?,
            output_dense: candle_nn::linear(
                config.intermediate_size,
                config.hidden_size,
                vb.pp("output.dense"),
            )?,
            output_layernorm: candle_nn::layer_norm(
                config.hidden_size,
                config.layer_norm_eps,
                vb.pp("output.LayerNorm"),
            )?,

            head_size: config.attention_head_size(),
        })
    }

    fn forward(
        &self,
        hidden_states: &Tensor,
        attention_mask: Option<&Tensor>,
        num_attention_heads: usize,
    ) -> Result<Tensor, candle_core::Error> {
        let (batch_size, seq_len, _) = hidden_states.dims3()?;

        // Self-attention computation.
        let query_layer = self.attention_query.forward(hidden_states)?;
        let key_layer = self.attention_key.forward(hidden_states)?;
        let value_layer = self.attention_value.forward(hidden_states)?;

        // Reshape for multi-head attention: [batch, seq_len, all_head_size] -> [batch, heads, seq_len, head_dim]
        let query_layer = query_layer
            .reshape((batch_size, seq_len, num_attention_heads, self.head_size))?
            .transpose(1, 2)?
            .contiguous()?;
        let key_layer = key_layer
            .reshape((batch_size, seq_len, num_attention_heads, self.head_size))?
            .transpose(1, 2)?
            .contiguous()?;
        let value_layer = value_layer
            .reshape((batch_size, seq_len, num_attention_heads, self.head_size))?
            .transpose(1, 2)?
            .contiguous()?;

        // Compute attention scores: [batch, heads, seq_len, seq_len]
        let key_t = key_layer.transpose(2, 3)?.contiguous()?;
        let attention_scores = query_layer.matmul(&key_t)?;

        // Scale and apply mask if provided.
        let scaled_attention_scores = (attention_scores / (self.head_size as f64).sqrt())?;

        let masked_attention_scores = match attention_mask {
            Some(mask) => {
                // Expand [batch, seq_len] -> broadcastable against scores
                // [batch, heads, seq_q, seq_k]. Padded positions (0 in mask) become a
                // large negative bias so they receive no attention mass.
                let ones = Tensor::ones_like(mask)?;
                let pad_indicator = (&ones - mask)?; // [B, S]
                let neg_bias = Tensor::full(-1e9f32, mask.dims(), mask.device())?;
                let bias = (pad_indicator.broadcast_mul(&neg_bias))?; // [B, S]
                // Explicitly shape to [B, 1, 1, S]; broadcasts over heads and query pos.
                let expanded = bias.reshape((batch_size, 1, 1, seq_len))?;
                scaled_attention_scores.broadcast_add(&expanded)?
            }
            None => scaled_attention_scores,
        };

        // Softmax over last dimension.
        let attention_probs =
            candle_nn::ops::softmax(&masked_attention_scores, candle_core::D::Minus1)?;

        // Apply attention to values: [batch, heads, seq_len, head_dim]
        let context_layer = attention_probs.matmul(&value_layer)?;

        // Reshape back and project output.
        let context_layer = context_layer.transpose(1, 2)?.contiguous()?.reshape((
            batch_size,
            seq_len,
            num_attention_heads * self.head_size,
        ))?;
        let attention_output = self.attention_output_dense.forward(&context_layer)?;

        // Add & norm residual connection.
        let attention_output = (hidden_states + attention_output)?;
        let attention_output = self.attention_output_layernorm.forward(&attention_output)?;

        // Intermediate layer with GELU activation.
        let intermediate_output = self.intermediate_dense.forward(&attention_output)?;
        let intermediate_output = intermediate_output.gelu_erf()?;

        // Output projection and add & norm.
        let layer_output = self.output_dense.forward(&intermediate_output)?;
        let layer_output = (layer_output + attention_output)?;
        let layer_output = self.output_layernorm.forward(&layer_output)?;

        Ok(layer_output)
    }
}

/// The full BERT model with proper batched input handling.
pub struct BertModel {
    word_embeddings: candle_nn::Embedding,
    position_embeddings: candle_nn::Embedding,
    token_type_embeddings: candle_nn::Embedding,
    embeddings_layernorm: candle_nn::LayerNorm,

    layers: Vec<BertLayer>,
    config: BertConfig,
    pub(crate) device: Device,
}

impl BertModel {
    pub fn load(vb: VarBuilder, config: &BertConfig) -> Result<Self, candle_core::Error> {
        let word_embeddings = candle_nn::embedding(
            config.vocab_size,
            config.hidden_size,
            vb.pp("embeddings.word_embeddings"),
        )?;
        let position_embeddings = candle_nn::embedding(
            config.max_position_embeddings,
            config.hidden_size,
            vb.pp("embeddings.position_embeddings"),
        )?;
        let token_type_embeddings = candle_nn::embedding(
            config.type_vocab_size,
            config.hidden_size,
            vb.pp("embeddings.token_type_embeddings"),
        )?;
        let embeddings_layernorm = candle_nn::layer_norm(
            config.hidden_size,
            config.layer_norm_eps,
            vb.pp("embeddings.LayerNorm"),
        )?;

        let mut layers = Vec::with_capacity(config.num_hidden_layers);
        for i in 0..config.num_hidden_layers {
            layers.push(BertLayer::load(
                vb.pp(format!("encoder.layer.{}", i)),
                config,
            )?);
        }

        Ok(Self {
            word_embeddings,
            position_embeddings,
            token_type_embeddings,
            embeddings_layernorm,
            layers,
            config: config.clone(),
            device: vb.device().clone(),
        })
    }

    /// Forward pass with proper batched input handling.
    pub fn forward(
        &self,
        input_ids: &Tensor,
        token_type_ids: &Tensor,
        attention_mask: Option<&Tensor>,
    ) -> Result<Tensor, candle_core::Error> {
        let (_, seq_len) = input_ids.dims2()?;

        // Word embeddings: [batch, seq_len] -> [batch, seq_len, hidden]
        let word_embeds = self.word_embeddings.forward(input_ids)?;

        // Position embeddings with proper 3D shape.
        let position_ids: Vec<u32> = (0..seq_len as u32).collect();
        let position_ids_tensor = Tensor::new(&position_ids[..], &self.device)?;
        let pos_embeds = self.position_embeddings.forward(&position_ids_tensor)?;

        // Token type embeddings.
        let token_type_embeds = self.token_type_embeddings.forward(token_type_ids)?;

        // Combine and normalize.
        let mut embeddings = (&word_embeds + &token_type_embeds)?;
        embeddings = embeddings.broadcast_add(&pos_embeds.unsqueeze(0)?)?;
        embeddings = self.embeddings_layernorm.forward(&embeddings)?;

        // Prepare attention mask for broadcasting: [batch, seq_len] -> [batch, 1, 1, seq_len].
        // Padded positions (0) become a large negative bias so they never receive
        // attention probability mass.
        // Pass the raw [batch, seq_len] mask; each layer expands it to
        // [batch, 1, 1, seq_len] for its attention scores. Padded positions (0)
        // become a large negative bias so they never receive attention mass.
        let masked_attention = match attention_mask {
            Some(mask) => Some(mask.to_dtype(candle_core::DType::F32)?),
            None => None,
        };
        // Run through transformer layers.
        let mut hidden_states = embeddings;
        for layer in &self.layers {
            hidden_states = layer.forward(
                &hidden_states,
                masked_attention.as_ref(),
                self.config.num_attention_heads,
            )?;
        }

        Ok(hidden_states)
    }
}
