//! The pinned model manifest (WP-06 task 1; design §9.2).
//!
//! Records identity, revision, digests and the full recipe so vectors are never
//! mixed on dimension alone. Only qualified recipes live here.

use std::collections::HashMap;

use crate::embeddings::artifacts::ModelArtifact;
use crate::embeddings::recipe::{ChunkingPolicy, Normalization, Pooling};

/// The single supported model: intfloat/multilingual-e5-small (CPU F32).
pub const E5_SMALL_ID: &str = "intfloat/multilingual-e5-small";

/// Pinned revision of the source repository at pin time.
pub const E5_SMALL_REVISION: &str = "614241f622f53c4eeff9890bdc4f31cfecc418b3";

/// MIT license (Hugging Face cardData) — redistribution permitted; weights are
/// fetched explicitly, never bundled.
pub const E5_SMALL_LICENSE: &str = "MIT";

const SOURCE_BASE_URL: &str = "https://huggingface.co/intfloat/multilingual-e5-small/resolve/main";

/// The pinned artifact set for the supported model (SHA-256 from manifest).
pub fn e5_small_artifact() -> ModelArtifact {
    let mut digests = HashMap::new();
    // Fetched 2026-09-18; see plans/models/e5-small-manifest.md.
    digests.insert(
        "config.json".into(),
        "69137736cab8b8903a07fe8afaafdda25aac55415a12a55d1bffa9f581abf959".into(),
    );
    digests.insert(
        "tokenizer_config.json".into(),
        "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b".into(),
    );
    digests.insert(
        "special_tokens_map.json".into(),
        "d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7".into(),
    );
    digests.insert(
        "tokenizer.json".into(),
        "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39".into(),
    );
    digests.insert(
        "sentencepiece.bpe.model".into(),
        "cfc8146abe2a0488e9e2a0c56de7952f7c11ab059eca145a0a727afce0db2865".into(),
    );
    digests.insert(
        "model.safetensors".into(),
        "1a55775f53449dac10a2bcbc312469fac40b96d53198c407081a831f81c98477".into(),
    );

    ModelArtifact {
        id: E5_SMALL_ID.into(),
        revision: E5_SMALL_REVISION.into(),
        license: E5_SMALL_LICENSE.into(),
        source_base_url: SOURCE_BASE_URL.into(),
        digests,
    }
}

/// The full recipe for the supported model (design §9.1/§9.2).
pub struct ModelRecipe {
    pub id: String,
    pub revision: String,
    /// Architecture adapter version — bump whenever adapter code changes semantics.
    pub adapter_version: u32,
    pub output_dim: usize,
    pub max_tokens: usize,
    pub query_prefix: &'static str,
    pub passage_prefix: &'static str,
    pub pooling: Pooling,
    pub normalization: Normalization,
    pub chunking: ChunkingPolicy,
}

/// The qualified E5-small recipe. This is the ONLY model ltmrs supports today;
/// adding a model requires a new entry here plus qualification fixtures.
pub fn e5_small_recipe() -> ModelRecipe {
    ModelRecipe {
        id: E5_SMALL_ID.into(),
        revision: E5_SMALL_REVISION.into(),
        adapter_version: 1,
        output_dim: 384,
        max_tokens: 512,
        query_prefix: "query: ",
        passage_prefix: "passage: ",
        pooling: Pooling::MaskedMean,
        normalization: Normalization::L2 { epsilon: 1e-12 },
        chunking: ChunkingPolicy::GreedyUnits { max_tokens: 512 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipe_matches_manifest_identity() {
        let artifact = e5_small_artifact();
        let recipe = e5_small_recipe();
        assert_eq!(artifact.id, recipe.id);
        assert_eq!(artifact.revision, recipe.revision);
        // The digests are the pin: a change here invalidates qualification.
        assert_eq!(artifact.digests.len(), 6);
    }

    #[test]
    fn recipe_is_the_e5_small_reference() {
        let r = e5_small_recipe();
        assert_eq!(r.query_prefix, "query: ");
        assert_eq!(r.passage_prefix, "passage: ");
        assert_eq!(r.max_tokens, 512);
        assert_eq!(r.output_dim, 384);
        assert!(matches!(r.pooling, Pooling::MaskedMean));
    }
}
