//! The qualified E5-small Candle adapter (WP-06 tasks 2, 3, 10).
//!
//! Implements the pinned reference recipe exactly: XLM-RoBERTa tokenizer,
//! BertModel backbone, query/passage prefixes, attention-mask-aware mean
//! pooling and L2 normalization. Unsupported recipes are rejected with an
//! actionable message — never a best-effort load of arbitrary weights.

use std::sync::Arc;

use tokenizers::Tokenizer;

use crate::bert_impl::BertModel;
use crate::manifest::ModelRecipe;
use crate::recipe::Role;
use ltmrs_domain::id::ModelFingerprint;

mod adapter;
#[cfg(test)]
mod e5_tests;

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
