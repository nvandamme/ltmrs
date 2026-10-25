//! MCP memory-tool argument parsers (moved verbatim from `mcp.rs`).

use rmcp::ErrorData as McpError;
use serde_json::{Map, Value};

use super::parse_fields::{
    bool_field, f64_field, opt_bool_field, project_field, require_str, response_format_field,
    str_field, usize_field,
};
use ltmrs_compat::lemma::tool_args::{
    MemoryAddArgs, MemoryAuditArgs, MemoryFeedbackArgs, MemoryForgetArgs, MemoryLibraryArgs,
    MemoryMergeArgs, MemoryReadArgs, MemoryRelateArgs, MemoryStatsArgs, MemoryUpdateArgs,
    SemanticSearchArgs,
};

pub(crate) fn parse_memory_read(args: &Map<String, Value>) -> Result<MemoryReadArgs, McpError> {
    Ok(MemoryReadArgs {
        project: project_field(args, "project")?,
        query: str_field(args, "query")?.map(|s| s.to_string()),
        id: str_field(args, "id")?.map(|s| s.to_string()),
        context: str_field(args, "context")?.map(|s| s.to_string()),
        all: bool_field(args, "all")?,
        ids: match args.get("ids") {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) => Some(
                a.iter()
                    .map(|x| {
                        x.as_str()
                            .map(|s| s.to_string())
                            .ok_or_else(|| McpError::invalid_params("ids must be strings", None))
                    })
                    .collect::<Result<_, _>>()?,
            ),
            Some(_) => {
                return Err(McpError::invalid_params(
                    "ids must be an array of strings",
                    None,
                ));
            }
        },
        min_confidence: f64_field(args, "minConfidence")?,
        after_date: str_field(args, "afterDate")?.map(|s| s.to_string()),
        before_date: str_field(args, "beforeDate")?.map(|s| s.to_string()),
        limit: usize_field(args, "limit")?,
        offset: usize_field(args, "offset")?,
        response_format: response_format_field(args, "response_format")?,
        expand_graph: bool_field(args, "expand_graph")?,
        explain: bool_field(args, "explain")?,
    })
}

/// Parse the frozen evidence sub-object (memory_add): closed shape —
/// unknown keys error like top-level args instead of silently dropping
/// hints; wrong-typed fields name the field instead of defaulting.
pub(crate) fn parse_evidence_object(
    o: &Map<String, Value>,
) -> Result<ltmrs_compat::lemma::tool_args::MemoryEvidence, McpError> {
    for key in o.keys() {
        if !matches!(key.as_str(), "file" | "symbol" | "snippet") {
            return Err(McpError::invalid_params(
                format!("unknown evidence field: {key}"),
                None,
            ));
        }
    }
    let file = o
        .get("file")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpError::invalid_params("evidence.file is required", None))?
        .to_string();
    let snippet = o
        .get("snippet")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpError::invalid_params("evidence.snippet is required", None))?
        .to_string();
    let symbol = match o.get("symbol") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.to_string()),
        Some(_) => {
            return Err(McpError::invalid_params(
                "evidence.symbol must be a string",
                None,
            ));
        }
    };
    Ok(ltmrs_compat::lemma::tool_args::MemoryEvidence {
        file,
        symbol,
        snippet,
    })
}

pub(crate) fn parse_memory_add(args: &Map<String, Value>) -> Result<MemoryAddArgs, McpError> {
    let fragment = require_str(args, "fragment")?;
    let evidence = args
        .get("evidence")
        .map(|v| match v {
            Value::Null => Ok(None),
            Value::Object(o) => parse_evidence_object(o).map(Some),
            _ => Err(McpError::invalid_params("evidence must be an object", None)),
        })
        .transpose()?
        .flatten();
    Ok(MemoryAddArgs {
        fragment,
        title: str_field(args, "title")?.map(|s| s.to_string()),
        description: str_field(args, "description")?.map(|s| s.to_string()),
        project: project_field(args, "project")?,
        source: str_field(args, "source")?.map(|s| s.to_string()),
        confirm: bool_field(args, "confirm")?,
        fragment_type: str_field(args, "type")?.map(|s| s.to_string()),
        evidence,
    })
}

pub(crate) fn parse_memory_update(args: &Map<String, Value>) -> Result<MemoryUpdateArgs, McpError> {
    Ok(MemoryUpdateArgs {
        id: require_str(args, "id")?,
        title: str_field(args, "title")?.map(|s| s.to_string()),
        fragment: str_field(args, "fragment")?.map(|s| s.to_string()),
        confidence: f64_field(args, "confidence")?,
    })
}

pub(crate) fn parse_memory_feedback(
    args: &Map<String, Value>,
) -> Result<MemoryFeedbackArgs, McpError> {
    // Absent/null and wrong-typed fail distinctly: "required" means the
    // caller omitted it, "must be a boolean" means they sent garbage.
    // (bool_field would silently default absent to false — wrong here.)
    let useful = match args.get("useful") {
        None | Some(Value::Null) => {
            return Err(McpError::invalid_params("useful is required", None));
        }
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(McpError::invalid_params("useful must be a boolean", None));
        }
    };
    Ok(MemoryFeedbackArgs {
        id: require_str(args, "id")?,
        useful,
    })
}

pub(crate) fn parse_memory_forget(args: &Map<String, Value>) -> Result<MemoryForgetArgs, McpError> {
    Ok(MemoryForgetArgs {
        id: require_str(args, "id")?,
        consolidate: bool_field(args, "consolidate")?,
        invalidate: bool_field(args, "invalidate")?,
    })
}

pub(crate) fn parse_memory_merge(args: &Map<String, Value>) -> Result<MemoryMergeArgs, McpError> {
    let ids = args
        .get("ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| McpError::invalid_params("ids is required", None))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| McpError::invalid_params("ids must be strings", None))
        })
        .collect::<Result<_, _>>()?;
    Ok(MemoryMergeArgs {
        ids,
        title: require_str(args, "title")?,
        fragment: require_str(args, "fragment")?,
        project: project_field(args, "project")?,
        consolidate: bool_field(args, "consolidate")?,
    })
}

pub(crate) fn parse_memory_relate(args: &Map<String, Value>) -> Result<MemoryRelateArgs, McpError> {
    let relation_type = require_str(args, "type")?;
    if ltmrs_domain::relation::RelationType::parse(&relation_type).is_none() {
        return Err(McpError::invalid_params(
            format!("invalid relation type: {relation_type}"),
            None,
        ));
    }
    Ok(MemoryRelateArgs {
        source_id: require_str(args, "sourceId")?,
        target_id: require_str(args, "targetId")?,
        relation_type,
        note: str_field(args, "note")?.map(|s| s.to_string()),
    })
}

pub(crate) fn parse_memory_stats(args: &Map<String, Value>) -> Result<MemoryStatsArgs, McpError> {
    Ok(MemoryStatsArgs {
        project: project_field(args, "project")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

pub(crate) fn parse_memory_audit(args: &Map<String, Value>) -> Result<MemoryAuditArgs, McpError> {
    Ok(MemoryAuditArgs {
        response_format: response_format_field(args, "response_format")?,
    })
}

pub(crate) fn parse_memory_library(
    args: &Map<String, Value>,
) -> Result<MemoryLibraryArgs, McpError> {
    Ok(MemoryLibraryArgs {
        project: project_field(args, "project")?,
        focus: str_field(args, "focus")?.map(|s| s.to_string()),
        limit: usize_field(args, "limit")?,
        offset: usize_field(args, "offset")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

pub(crate) fn parse_semantic_search(
    args: &Map<String, Value>,
) -> Result<SemanticSearchArgs, McpError> {
    Ok(SemanticSearchArgs {
        query: require_str(args, "query")?,
        project: project_field(args, "project")?,
        top_k: usize_field(args, "topK")?,
        offset: usize_field(args, "offset")?,
        hybrid: opt_bool_field(args, "hybrid")?,
        explain: bool_field(args, "explain")?,
        response_format: response_format_field(args, "response_format")?,
    })
}
