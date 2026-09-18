//! Model recipe types (WP-06; design §9). A recipe fully specifies how text
//! becomes a vector: prefixes, window limit, pooling, normalization and the
//! chunking policy. The adapter validates against it before running anything.

/// Input role for prefix application (query/passage asymmetry is part of the
/// E5-small reference recipe — T-EMB-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Query,
    Passage,
}

/// Pooling over token hidden states. Only masked mean is qualified today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Attention-mask-aware mean pooling (1_Pooling/config.json).
    MaskedMean,
}

/// Output normalization applied after pooling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Normalization {
    /// L2 normalize with an epsilon guard against zero vectors.
    L2 { epsilon: f32 },
}

/// Deterministic chunking policy for memories exceeding the model window (RV-10).
/// Units are paragraphs/lines; packing is greedy and token-counted, never a
/// character heuristic. Oversized single units get disclosed hard splits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkingPolicy {
    /// Greedy unit packing under the model's token window.
    GreedyUnits { max_tokens: usize },
}

impl Role {
    pub fn prefix<'a>(&self, query_prefix: &'a str, passage_prefix: &'a str) -> &'a str {
        match self {
            Self::Query => query_prefix,
            Self::Passage => passage_prefix,
        }
    }
}
