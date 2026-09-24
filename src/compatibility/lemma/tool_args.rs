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

/// The 26 tools (11 WP-08 + 15 WP-09).
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
    GuideGet,
    GuidePractice,
    GuideCreate,
    GuideDistill,
    GuideUpdate,
    GuideForget,
    GuideMerge,
    SessionStart,
    SessionAttempt,
    SessionEnd,
    SessionStats,
    SuggestionRespond,
    ConflictScan,
    ProactiveAnalysis,
    ProjectAnalytics,
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
            Self::GuideGet => "guide_get",
            Self::GuidePractice => "guide_practice",
            Self::GuideCreate => "guide_create",
            Self::GuideDistill => "guide_distill",
            Self::GuideUpdate => "guide_update",
            Self::GuideForget => "guide_forget",
            Self::GuideMerge => "guide_merge",
            Self::SessionStart => "session_start",
            Self::SessionAttempt => "session_attempt",
            Self::SessionEnd => "session_end",
            Self::SessionStats => "session_stats",
            Self::SuggestionRespond => "suggestion_respond",
            Self::ConflictScan => "conflict_scan",
            Self::ProactiveAnalysis => "proactive_analysis",
            Self::ProjectAnalytics => "project_analytics",
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
            "guide_get" => Self::GuideGet,
            "guide_practice" => Self::GuidePractice,
            "guide_create" => Self::GuideCreate,
            "guide_distill" => Self::GuideDistill,
            "guide_update" => Self::GuideUpdate,
            "guide_forget" => Self::GuideForget,
            "guide_merge" => Self::GuideMerge,
            "session_start" => Self::SessionStart,
            "session_attempt" => Self::SessionAttempt,
            "session_end" => Self::SessionEnd,
            "session_stats" => Self::SessionStats,
            "suggestion_respond" => Self::SuggestionRespond,
            "conflict_scan" => Self::ConflictScan,
            "proactive_analysis" => Self::ProactiveAnalysis,
            "project_analytics" => Self::ProjectAnalytics,
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

/// guide_get arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuideGetArgs {
    pub category: Option<String>,
    pub guide: Option<String>,
    pub task: Option<String>,
    pub response_format: Option<ResponseFormat>,
}

/// guide_practice arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuidePracticeArgs {
    pub guide: String,
    pub category: String,
    pub description: Option<String>,
    pub contexts: Vec<String>,
    pub learnings: Vec<String>,
    pub outcome: Option<String>,
}

/// guide_create arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuideCreateArgs {
    pub guide: String,
    pub category: String,
    pub description: String,
    pub contexts: Vec<String>,
    pub learnings: Vec<String>,
}

/// guide_distill arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuideDistillArgs {
    pub memory_id: String,
    pub guide: String,
    pub category: Option<String>,
}

/// guide_update arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuideUpdateArgs {
    pub guide: String,
    pub new_name: Option<String>,
    pub category: Option<String>,
    pub description: Option<String>,
    pub add_anti_patterns: Vec<String>,
    pub add_pitfalls: Vec<String>,
    pub add_depends_on: Vec<String>,
    pub add_enables: Vec<String>,
    pub superseded_by: Option<String>,
    pub deprecated: bool,
}

/// guide_forget arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuideForgetArgs {
    pub guide: String,
}

/// guide_merge arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuideMergeArgs {
    pub guides: Vec<String>,
    pub guide: String,
    pub category: String,
    pub description: Option<String>,
    pub contexts: Option<Vec<String>>,
    pub learnings: Option<Vec<String>>,
}

/// session_start arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStartArgs {
    pub task_type: String,
    pub technologies: Vec<String>,
    pub initial_approach: Option<String>,
}

/// session_attempt arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionAttemptArgs {
    pub approach: String,
    pub outcome: String,
    pub critique: Option<String>,
    pub rationale: Option<String>,
    pub related_memory_id: Option<String>,
}

/// session_end arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionEndArgs {
    pub outcome: String,
    pub final_approach: Option<String>,
    pub lessons: Vec<String>,
}

/// session_stats arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsArgs {
    pub count: Option<usize>,
    pub response_format: Option<ResponseFormat>,
}

/// suggestion_respond arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestionRespondArgs {
    pub id: u64,
    pub action: String,
}

/// conflict_scan arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConflictScanArgs {
    pub project: Option<String>,
    pub response_format: Option<ResponseFormat>,
}

/// proactive_analysis arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProactiveAnalysisArgs {
    pub project: Option<String>,
    pub response_format: Option<ResponseFormat>,
}

/// project_analytics arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectAnalyticsArgs {
    pub project: Option<String>,
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
    GuideGet(GuideGetArgs),
    GuidePractice(GuidePracticeArgs),
    GuideCreate(GuideCreateArgs),
    GuideDistill(GuideDistillArgs),
    GuideUpdate(GuideUpdateArgs),
    GuideForget(GuideForgetArgs),
    GuideMerge(GuideMergeArgs),
    SessionStart(SessionStartArgs),
    SessionAttempt(SessionAttemptArgs),
    SessionEnd(SessionEndArgs),
    SessionStats(SessionStatsArgs),
    SuggestionRespond(SuggestionRespondArgs),
    ConflictScan(ConflictScanArgs),
    ProactiveAnalysis(ProactiveAnalysisArgs),
    ProjectAnalytics(ProjectAnalyticsArgs),
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
            Self::GuideGet(_) => ToolName::GuideGet,
            Self::GuidePractice(_) => ToolName::GuidePractice,
            Self::GuideCreate(_) => ToolName::GuideCreate,
            Self::GuideDistill(_) => ToolName::GuideDistill,
            Self::GuideUpdate(_) => ToolName::GuideUpdate,
            Self::GuideForget(_) => ToolName::GuideForget,
            Self::GuideMerge(_) => ToolName::GuideMerge,
            Self::SessionStart(_) => ToolName::SessionStart,
            Self::SessionAttempt(_) => ToolName::SessionAttempt,
            Self::SessionEnd(_) => ToolName::SessionEnd,
            Self::SessionStats(_) => ToolName::SessionStats,
            Self::SuggestionRespond(_) => ToolName::SuggestionRespond,
            Self::ConflictScan(_) => ToolName::ConflictScan,
            Self::ProactiveAnalysis(_) => ToolName::ProactiveAnalysis,
            Self::ProjectAnalytics(_) => ToolName::ProjectAnalytics,
        }
    }
}
