//! Typed legacy tool names and arguments (Lemma 0.21.0 compatibility).
//!
//! The MCP frontend validates raw JSON arguments against the frozen schema and
//! produces a typed [`ToolArgs`]. The daemon executes it. This keeps argument
//! parsing (a compatibility concern) separate from domain execution, and makes
//! the protocol boundary explicit and testable.
//!
//! Field names are the EXACT camelCase/snake_case forms from the frozen schema
//! (T-MCP-01). `additionalProperties: false` is enforced by the schema; the
//! frontend rejects unknown keys.

use serde::{Deserialize, Serialize};

/// The 11 WP-08 tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    MemoryRead,
    MemoryAdd,
    MemoryUpdate,
    MemoryFeedback,
    MemoryForget,
    MemoryMerge,
    MemoryRelate,
    MemoryStats,
    MemoryAudit,
    MemoryLibrary,
    SemanticSearch,
}

impl ToolName {
    /// The frozen wire name (matches the schema `name` field).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MemoryRead => "memory_read",
            Self::MemoryAdd => "memory_add",
            Self::MemoryUpdate => "memory_update",
            Self::MemoryFeedback => "memory_feedback",
            Self::MemoryForget => "memory_forget",
            Self::MemoryMerge => "memory_merge",
            Self::MemoryRelate => "memory_relate",
            Self::MemoryStats => "memory_stats",
            Self::MemoryAudit => "memory_audit",
            Self::MemoryLibrary => "memory_library",
            Self::SemanticSearch => "semantic_search",
        }
    }

    /// Parse a wire tool name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "memory_read" => Self::MemoryRead,
            "memory_add" => Self::MemoryAdd,
            "memory_update" => Self::MemoryUpdate,
            "memory_feedback" => Self::MemoryFeedback,
            "memory_forget" => Self::MemoryForget,
            "memory_merge" => Self::MemoryMerge,
            "memory_relate" => Self::MemoryRelate,
            "memory_stats" => Self::MemoryStats,
            "memory_audit" => Self::MemoryAudit,
            "memory_library" => Self::MemoryLibrary,
            "semantic_search" => Self::SemanticSearch,
            _ => return None,
        })
    }
}

/// The response format selection (frozen enum: markdown | json).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseFormat {
    #[default]
    Markdown,
    Json,
}

impl ResponseFormat {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "markdown" => Self::Markdown,
            "json" => Self::Json,
            _ => return None,
        })
    }
}

/// memory_read arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryReadArgs {
    pub project: Option<String>,
    pub query: Option<String>,
    pub id: Option<String>,
    pub context: Option<String>,
    pub all: bool,
    pub ids: Option<Vec<String>>,
    pub min_confidence: Option<f64>,
    pub after_date: Option<String>,
    pub before_date: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub response_format: Option<ResponseFormat>,
    pub expand_graph: bool,
    pub explain: bool,
}

/// memory_add arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryAddArgs {
    pub fragment: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub project: Option<String>,
    pub source: Option<String>,
    pub confirm: bool,
    pub fragment_type: Option<String>,
    pub evidence: Option<MemoryEvidence>,
}

/// memory_add evidence (frozen schema).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEvidence {
    pub file: String,
    pub symbol: Option<String>,
    pub snippet: String,
}

/// memory_update arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryUpdateArgs {
    pub id: String,
    pub title: Option<String>,
    pub fragment: Option<String>,
    pub confidence: Option<f64>,
}

/// memory_feedback arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryFeedbackArgs {
    pub id: String,
    pub useful: bool,
}

/// memory_forget arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryForgetArgs {
    pub id: String,
    pub consolidate: bool,
    pub invalidate: bool,
}

/// memory_merge arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryMergeArgs {
    pub ids: Vec<String>,
    pub title: String,
    pub fragment: String,
    pub project: Option<String>,
    pub consolidate: bool,
}

/// memory_relate arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRelateArgs {
    pub source_id: String,
    pub target_id: String,
    pub relation_type: String,
    pub note: Option<String>,
}

/// memory_stats arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryStatsArgs {
    pub project: Option<String>,
    pub response_format: Option<ResponseFormat>,
}

/// memory_audit arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryAuditArgs {
    pub response_format: Option<ResponseFormat>,
}

/// memory_library arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryLibraryArgs {
    pub project: Option<String>,
    pub focus: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub response_format: Option<ResponseFormat>,
}

/// semantic_search arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticSearchArgs {
    pub query: String,
    pub project: Option<String>,
    pub top_k: Option<usize>,
    pub offset: Option<usize>,
    pub hybrid: bool,
    pub explain: bool,
    pub response_format: Option<ResponseFormat>,
}

/// The typed arguments for one tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tool", content = "args", rename_all = "snake_case")]
pub enum ToolArgs {
    MemoryRead(MemoryReadArgs),
    MemoryAdd(MemoryAddArgs),
    MemoryUpdate(MemoryUpdateArgs),
    MemoryFeedback(MemoryFeedbackArgs),
    MemoryForget(MemoryForgetArgs),
    MemoryMerge(MemoryMergeArgs),
    MemoryRelate(MemoryRelateArgs),
    MemoryStats(MemoryStatsArgs),
    MemoryAudit(MemoryAuditArgs),
    MemoryLibrary(MemoryLibraryArgs),
    SemanticSearch(SemanticSearchArgs),
}

impl ToolArgs {
    /// The tool this argument set belongs to.
    pub fn tool(&self) -> ToolName {
        match self {
            Self::MemoryRead(_) => ToolName::MemoryRead,
            Self::MemoryAdd(_) => ToolName::MemoryAdd,
            Self::MemoryUpdate(_) => ToolName::MemoryUpdate,
            Self::MemoryFeedback(_) => ToolName::MemoryFeedback,
            Self::MemoryForget(_) => ToolName::MemoryForget,
            Self::MemoryMerge(_) => ToolName::MemoryMerge,
            Self::MemoryRelate(_) => ToolName::MemoryRelate,
            Self::MemoryStats(_) => ToolName::MemoryStats,
            Self::MemoryAudit(_) => ToolName::MemoryAudit,
            Self::MemoryLibrary(_) => ToolName::MemoryLibrary,
            Self::SemanticSearch(_) => ToolName::SemanticSearch,
        }
    }
}
