//! MCP guide-tool argument parsers (moved verbatim from `mcp.rs`).

use rmcp::ErrorData as McpError;
use serde_json::{Map, Value};

use super::parse_fields::{
    bool_field, require_str, response_format_field, str_array_field, str_field,
};
use ltmrs_compat::lemma::tool_args::{
    GuideCreateArgs, GuideDistillArgs, GuideForgetArgs, GuideGetArgs, GuideMergeArgs,
    GuidePracticeArgs, GuideUpdateArgs,
};

pub(crate) fn parse_guide_get(args: &Map<String, Value>) -> Result<GuideGetArgs, McpError> {
    Ok(GuideGetArgs {
        category: str_field(args, "category")?.map(|s| s.to_string()),
        guide: str_field(args, "guide")?.map(|s| s.to_string()),
        task: str_field(args, "task")?.map(|s| s.to_string()),
        response_format: response_format_field(args, "response_format")?,
    })
}

pub(crate) fn parse_guide_practice(
    args: &Map<String, Value>,
) -> Result<GuidePracticeArgs, McpError> {
    Ok(GuidePracticeArgs {
        guide: require_str(args, "guide")?,
        category: require_str(args, "category")?,
        description: str_field(args, "description")?.map(|s| s.to_string()),
        contexts: str_array_field(args, "contexts")?,
        learnings: str_array_field(args, "learnings")?,
        outcome: str_field(args, "outcome")?.map(|s| s.to_string()),
    })
}

pub(crate) fn parse_guide_create(args: &Map<String, Value>) -> Result<GuideCreateArgs, McpError> {
    Ok(GuideCreateArgs {
        guide: require_str(args, "guide")?,
        category: require_str(args, "category")?,
        description: require_str(args, "description")?,
        contexts: str_array_field(args, "contexts")?,
        learnings: str_array_field(args, "learnings")?,
    })
}

pub(crate) fn parse_guide_distill(args: &Map<String, Value>) -> Result<GuideDistillArgs, McpError> {
    Ok(GuideDistillArgs {
        memory_id: require_str(args, "memory_id")?,
        guide: require_str(args, "guide")?,
        category: str_field(args, "category")?.map(|s| s.to_string()),
    })
}

pub(crate) fn parse_guide_update(args: &Map<String, Value>) -> Result<GuideUpdateArgs, McpError> {
    Ok(GuideUpdateArgs {
        guide: require_str(args, "guide")?,
        new_name: str_field(args, "new_name")?.map(|s| s.to_string()),
        category: str_field(args, "category")?.map(|s| s.to_string()),
        description: str_field(args, "description")?.map(|s| s.to_string()),
        add_anti_patterns: str_array_field(args, "add_anti_patterns")?,
        add_pitfalls: str_array_field(args, "add_pitfalls")?,
        add_depends_on: str_array_field(args, "add_depends_on")?,
        add_enables: str_array_field(args, "add_enables")?,
        superseded_by: str_field(args, "superseded_by")?.map(|s| s.to_string()),
        deprecated: bool_field(args, "deprecated")?,
    })
}

pub(crate) fn parse_guide_forget(args: &Map<String, Value>) -> Result<GuideForgetArgs, McpError> {
    Ok(GuideForgetArgs {
        guide: require_str(args, "guide")?,
    })
}

pub(crate) fn parse_guide_merge(args: &Map<String, Value>) -> Result<GuideMergeArgs, McpError> {
    Ok(GuideMergeArgs {
        guides: str_array_field(args, "guides")?,
        guide: require_str(args, "guide")?,
        category: require_str(args, "category")?,
        description: str_field(args, "description")?.map(|s| s.to_string()),
        contexts: {
            match args.get("contexts") {
                None | Some(Value::Null) => None,
                Some(Value::Array(a)) => Some(
                    a.iter()
                        .map(|x| {
                            x.as_str().map(|s| s.to_string()).ok_or_else(|| {
                                McpError::invalid_params("contexts must be strings", None)
                            })
                        })
                        .collect::<Result<_, _>>()?,
                ),
                Some(_) => {
                    return Err(McpError::invalid_params(
                        "contexts must be an array of strings",
                        None,
                    ));
                }
            }
        },
        learnings: {
            match args.get("learnings") {
                None | Some(Value::Null) => None,
                Some(Value::Array(a)) => Some(
                    a.iter()
                        .map(|x| {
                            x.as_str().map(|s| s.to_string()).ok_or_else(|| {
                                McpError::invalid_params("learnings must be strings", None)
                            })
                        })
                        .collect::<Result<_, _>>()?,
                ),
                Some(_) => {
                    return Err(McpError::invalid_params(
                        "learnings must be an array of strings",
                        None,
                    ));
                }
            }
        },
    })
}
