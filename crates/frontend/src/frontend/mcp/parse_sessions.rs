//! MCP session/suggestion/intelligence/backup argument parsers (moved verbatim from `mcp.rs`).

use rmcp::ErrorData as McpError;
use serde_json::{Map, Value};

use super::parse_fields::{
    opt_bool_field, project_field, require_str, response_format_field, str_array_field, str_field,
    usize_field,
};
use ltmrs_compat::lemma::tool_args::{
    BackupCreateArgs, BackupPreviewArgs, BackupRestoreArgs, ConflictScanArgs,
    ProactiveAnalysisArgs, ProjectAnalyticsArgs, SessionAttemptArgs, SessionEndArgs,
    SessionStartArgs, SessionStatsArgs, SuggestionRespondArgs,
};

pub(crate) fn parse_session_start(args: &Map<String, Value>) -> Result<SessionStartArgs, McpError> {
    Ok(SessionStartArgs {
        task_type: require_str(args, "task_type")?,
        technologies: str_array_field(args, "technologies")?,
        initial_approach: str_field(args, "initial_approach")?.map(|s| s.to_string()),
    })
}

pub(crate) fn parse_session_attempt(
    args: &Map<String, Value>,
) -> Result<SessionAttemptArgs, McpError> {
    Ok(SessionAttemptArgs {
        approach: require_str(args, "approach")?,
        outcome: require_str(args, "outcome")?,
        critique: str_field(args, "critique")?.map(|s| s.to_string()),
        rationale: str_field(args, "rationale")?.map(|s| s.to_string()),
        related_memory_id: str_field(args, "related_memory_id")?.map(|s| s.to_string()),
    })
}

pub(crate) fn parse_session_end(args: &Map<String, Value>) -> Result<SessionEndArgs, McpError> {
    Ok(SessionEndArgs {
        outcome: require_str(args, "outcome")?,
        final_approach: str_field(args, "final_approach")?.map(|s| s.to_string()),
        lessons: str_array_field(args, "lessons")?,
    })
}

pub(crate) fn parse_session_stats(args: &Map<String, Value>) -> Result<SessionStatsArgs, McpError> {
    Ok(SessionStatsArgs {
        count: usize_field(args, "count")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

pub(crate) fn parse_suggestion_respond(
    args: &Map<String, Value>,
) -> Result<SuggestionRespondArgs, McpError> {
    Ok(SuggestionRespondArgs {
        id: args
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| McpError::invalid_params("id is required", None))?,
        action: require_str(args, "action")?,
    })
}

pub(crate) fn parse_conflict_scan(args: &Map<String, Value>) -> Result<ConflictScanArgs, McpError> {
    Ok(ConflictScanArgs {
        project: project_field(args, "project")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

pub(crate) fn parse_proactive_analysis(
    args: &Map<String, Value>,
) -> Result<ProactiveAnalysisArgs, McpError> {
    Ok(ProactiveAnalysisArgs {
        project: project_field(args, "project")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

pub(crate) fn parse_project_analytics(
    args: &Map<String, Value>,
) -> Result<ProjectAnalyticsArgs, McpError> {
    Ok(ProjectAnalyticsArgs {
        project: project_field(args, "project")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

/// Parse backup_preview args (`path` optional at the wire level, required
/// at execution).
pub(crate) fn parse_backup_preview(
    args: &Map<String, Value>,
) -> Result<BackupPreviewArgs, McpError> {
    Ok(BackupPreviewArgs {
        path: str_field(args, "path")?.map(|s| s.to_string()),
    })
}

/// Parse backup_restore args (both optional at the wire level; execution
/// requires an unused token plus explicit confirmation).
pub(crate) fn parse_backup_restore(
    args: &Map<String, Value>,
) -> Result<BackupRestoreArgs, McpError> {
    Ok(BackupRestoreArgs {
        confirmation_token: str_field(args, "confirmation_token")?.map(|s| s.to_string()),
        confirm: opt_bool_field(args, "confirm")?,
    })
}

pub(crate) fn parse_backup_create(args: &Map<String, Value>) -> Result<BackupCreateArgs, McpError> {
    Ok(BackupCreateArgs {
        directory: str_field(args, "directory")?.map(|s| s.to_string()),
    })
}
