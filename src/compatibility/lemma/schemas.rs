//! Frozen tool schemas (Lemma 0.21.0 compatibility).
//!
//! The tool list, input schemas, descriptions, annotations and output schemas
//! are read from the frozen capture `tools_lemma_0_21_0.json`. The frontend
//! serves these verbatim so `tools/list` matches the pinned baseline exactly
//! (T-MCP-01): no missing/renamed tool, no schema drift, no extra mandatory
//! legacy input.
//!
//! Design 11.1: "A --compat=lemma adapter uses the frozen schemas directly
//! rather than trusting generated Rust schemas to match all
//! nullability/default/camelCase details."

use serde_json::{Map, Value};

/// The raw frozen tool definitions for the 11 WP-08 tools.
const FROZEN_TOOLS_JSON: &str = include_str!("tools_lemma_0_21_0.json");

/// A frozen tool definition (name, description, schema, annotations, output).
#[derive(Debug, Clone, PartialEq)]
pub struct FrozenTool {
    pub name: String,
    pub description: String,
    pub input_schema: Map<String, Value>,
    /// The four annotation hints (present in the frozen baseline).
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub idempotent_hint: bool,
    pub open_world_hint: bool,
    /// The frozen output schema (for structured responses).
    pub output_schema: Option<Map<String, Value>>,
}

/// Load the 11 frozen WP-08 tool definitions.
///
/// Returns them in the frozen baseline order.
pub fn frozen_tools() -> Vec<FrozenTool> {
    let v: Value =
        serde_json::from_str(FROZEN_TOOLS_JSON).expect("frozen tool schemas must be valid JSON");
    let arr = v.as_array().expect("frozen tools must be an array");
    arr.iter().map(parse_tool).collect()
}

fn parse_tool(v: &Value) -> FrozenTool {
    let name = v["name"].as_str().expect("tool name").to_string();
    let description = v["description"].as_str().unwrap_or("").to_string();
    let input_schema = v["inputSchema"]
        .as_object()
        .expect("inputSchema must be an object")
        .clone();
    let ann = &v["annotations"];
    let output_schema = v["outputSchema"].as_object().cloned();
    FrozenTool {
        name,
        description,
        input_schema,
        read_only_hint: ann["readOnlyHint"].as_bool().unwrap_or(false),
        destructive_hint: ann["destructiveHint"].as_bool().unwrap_or(false),
        idempotent_hint: ann["idempotentHint"].as_bool().unwrap_or(false),
        open_world_hint: ann["openWorldHint"].as_bool().unwrap_or(false),
        output_schema,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_exactly_eleven_wp08_tools() {
        let tools = frozen_tools();
        assert_eq!(tools.len(), 11, "WP-08 serves exactly 11 tools");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"memory_read"));
        assert!(names.contains(&"memory_add"));
        assert!(names.contains(&"memory_update"));
        assert!(names.contains(&"memory_feedback"));
        assert!(names.contains(&"memory_forget"));
        assert!(names.contains(&"memory_merge"));
        assert!(names.contains(&"memory_relate"));
        assert!(names.contains(&"memory_stats"));
        assert!(names.contains(&"memory_audit"));
        assert!(names.contains(&"memory_library"));
        assert!(names.contains(&"semantic_search"));
    }

    #[test]
    fn schemas_match_frozen_baseline() {
        // memory_add requires only `fragment` (T-MCP-01: no extra mandatory input).
        let tools = frozen_tools();
        let add = tools.iter().find(|t| t.name == "memory_add").unwrap();
        let required = add.input_schema["required"]
            .as_array()
            .expect("memory_add has required");
        assert_eq!(required, &vec![Value::String("fragment".into())]);
        // `confirm` defaults to false (the privacy override, DEV-002).
        assert_eq!(add.input_schema["properties"]["confirm"]["default"], false);

        // semantic_search is read-only and idempotent (T-MCP-01 annotations).
        let ss = tools.iter().find(|t| t.name == "semantic_search").unwrap();
        assert!(ss.read_only_hint, "semantic_search must be read-only");
        assert!(ss.idempotent_hint, "semantic_search must be idempotent");

        // memory_forget is destructive.
        let forget = tools.iter().find(|t| t.name == "memory_forget").unwrap();
        assert!(forget.destructive_hint, "memory_forget must be destructive");
    }

    #[test]
    fn output_schemas_present() {
        let tools = frozen_tools();
        for t in &tools {
            assert!(
                t.output_schema.is_some(),
                "{} must have an output schema",
                t.name
            );
        }
    }
}
