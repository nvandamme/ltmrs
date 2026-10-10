//! MCP argument field helpers (moved verbatim from `mcp.rs`).

use rmcp::ErrorData as McpError;
use serde_json::{Map, Value};

use ltmrs_compat::lemma::tool_args::ResponseFormat;

/// Allowed argument keys per tool, derived from the served input schemas
/// (frozen baseline + native tools). `route_tool` rejects anything else.
static ALLOWED_ARGUMENTS: std::sync::LazyLock<std::collections::HashMap<String, Vec<String>>> =
    std::sync::LazyLock::new(|| {
        let mut map = std::collections::HashMap::new();
        for tool in ltmrs_compat::lemma::schemas::frozen_tools() {
            let keys = tool
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            map.insert(tool.name, keys);
        }
        map.insert("backup_create".to_string(), vec!["directory".to_string()]);
        map.insert("backup_preview".to_string(), vec!["path".to_string()]);
        map.insert(
            "backup_restore".to_string(),
            vec!["confirmation_token".to_string(), "confirm".to_string()],
        );
        map
    });

pub(crate) fn allowed_arguments(name: &str) -> Option<Vec<String>> {
    ALLOWED_ARGUMENTS.get(name).cloned()
}

// Explicit nulls are absent (lenient: hosts send null for "not set").
// Any other present-but-wrong-typed value is an error, never a silent
// default: a string limit must not become "unbounded".
pub(crate) fn str_field<'a>(
    args: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a string"),
            None,
        )),
    }
}

pub(crate) fn bool_field(args: &Map<String, Value>, key: &str) -> Result<bool, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a boolean"),
            None,
        )),
    }
}

pub(crate) fn usize_field(args: &Map<String, Value>, key: &str) -> Result<Option<usize>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v) => Ok(Some(v as usize)),
            None => Err(McpError::invalid_params(
                format!("{key} must be an integer"),
                None,
            )),
        },
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be an integer"),
            None,
        )),
    }
}

pub(crate) fn f64_field(args: &Map<String, Value>, key: &str) -> Result<Option<f64>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(v) => Ok(Some(v)),
            None => Err(McpError::invalid_params(
                format!("{key} must be a number"),
                None,
            )),
        },
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a number"),
            None,
        )),
    }
}

pub(crate) fn response_format_field(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Option<ResponseFormat>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(ResponseFormat::parse(s).ok_or_else(|| {
            McpError::invalid_params(format!("{key} must be a known response format"), None)
        })?)),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a string"),
            None,
        )),
    }
}

pub(crate) fn require_str(args: &Map<String, Value>, key: &str) -> Result<String, McpError> {
    str_field(args, key)?
        .map(|s| s.to_string())
        .ok_or_else(|| McpError::invalid_params(format!("{key} is required"), None))
}

/// Optional boolean: absent/null means unset (caller default applies);
/// present values must be booleans.
pub(crate) fn opt_bool_field(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Option<bool>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a boolean"),
            None,
        )),
    }
}

/// Project scope normalized at the parse boundary (trimmed, basename,
/// lowercased): reads and writes meet on the canonical form instead of
/// comparing raw input against normalized storage.
pub(crate) fn project_field(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, McpError> {
    Ok(match str_field(args, key)? {
        None => None,
        Some(raw) => ltmrs_compat::lemma::tool_args::normalize_project(raw),
    })
}

pub(crate) fn str_array_field(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Vec<String>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| McpError::invalid_params(format!("{key} must be strings"), None))
            })
            .collect(),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be an array of strings"),
            None,
        )),
    }
}
