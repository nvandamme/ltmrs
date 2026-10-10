//! MCP route/parse/shape tests (moved verbatim from `mcp.rs`).

use super::handler::shape_result;
use super::parse_fields::allowed_arguments;
use super::parse_memory::parse_memory_feedback;
use super::{FrontendIdentity, LtmrsFrontend, frozen_tools, route_tool};
use ltmrs_compat::lemma::tool_args::ToolArgs;
use ltmrs_daemon::client::IpcClient;
use ltmrs_daemon::envelope::{DomainRequest, PROTOCOL_VERSION};
use ltmrs_domain::id::{ChannelId, FrontendId, OperationId};
use rmcp::model::CallToolRequestParams;
use serde_json::{Map, Value, json};
use uuid::Uuid;

fn args(map: Map<String, Value>) -> Option<Map<String, Value>> {
    Some(map)
}

#[test]
fn tools_carry_annotations_and_output_schema() {
    let tools = LtmrsFrontend::tools();
    let ss = tools.iter().find(|t| t.name == "semantic_search").unwrap();
    assert!(ss.annotations.as_ref().unwrap().read_only_hint == Some(true));
    assert!(
        ss.output_schema.is_some(),
        "semantic_search must expose output schema"
    );
}

#[test]
fn routes_memory_add() {
    let mut m = Map::new();
    m.insert("fragment".into(), json!("hello world"));
    let req = route_tool("memory_add", &args(m)).unwrap();
    match req {
        DomainRequest::ToolCall { tool } => {
            assert!(matches!(tool, ToolArgs::MemoryAdd(_)));
        }
        _ => panic!("expected ToolCall"),
    }
}

#[test]
fn memory_add_requires_fragment() {
    assert!(route_tool("memory_add", &None).is_err());
}

/// I2 strictness: a present-but-wrong-typed evidence.symbol errors
/// instead of silently dropping the hint.
#[test]
fn memory_add_rejects_non_string_evidence_symbol() {
    let mut m = Map::new();
    m.insert("fragment".into(), json!("hello world"));
    m.insert(
        "evidence".into(),
        json!({"file": "a.rs", "snippet": "x", "symbol": 42}),
    );
    assert!(route_tool("memory_add", &args(m)).is_err());
}

/// I2 strictness: unknown evidence fields error like unknown top-level
/// args instead of silently dropping hints.
#[test]
fn memory_add_rejects_unknown_evidence_field() {
    let mut m = Map::new();
    m.insert("fragment".into(), json!("hello world"));
    m.insert(
        "evidence".into(),
        json!({"file": "a.rs", "snippet": "x", "symobl": "f"}),
    );
    assert!(route_tool("memory_add", &args(m)).is_err());
}

/// I2 strictness: non-object evidence errors instead of storing the
/// memory with the evidence silently dropped.
#[test]
fn memory_add_rejects_non_object_evidence() {
    for bad in [json!("a.rs"), json!(42), json!(["a.rs"])] {
        let mut m = Map::new();
        m.insert("fragment".into(), json!("hello world"));
        m.insert("evidence".into(), bad);
        assert!(
            route_tool("memory_add", &args(m)).is_err(),
            "non-object evidence must error"
        );
    }
}

/// Null evidence stays absent (lenient); boolean evidence errors like
/// every other wrong-typed value.
#[test]
fn memory_add_null_evidence_absent_bool_errors() {
    let mut m = Map::new();
    m.insert("fragment".into(), json!("hello world"));
    m.insert("evidence".into(), Value::Null);
    assert!(route_tool("memory_add", &args(m)).is_ok());
    let mut m = Map::new();
    m.insert("fragment".into(), json!("hello world"));
    m.insert("evidence".into(), json!(true));
    assert!(route_tool("memory_add", &args(m)).is_err());
}

/// I2 strictness: a present-but-wrong-typed backup_restore confirm
/// errors instead of silently becoming unset.
#[test]
fn backup_restore_rejects_non_boolean_confirm() {
    let mut m = Map::new();
    m.insert("confirm".into(), json!("yes"));
    assert!(route_tool("backup_restore", &args(m)).is_err());
}

#[test]
fn routes_memory_read() {
    let req = route_tool("memory_read", &None).unwrap();
    assert!(matches!(req, DomainRequest::ToolCall { .. }));
}

#[test]
fn memory_feedback_requires_useful() {
    let mut m = Map::new();
    m.insert("id".into(), json!("abc"));
    assert!(route_tool("memory_feedback", &args(m)).is_err());
}

/// Wrong-typed `useful` names the type problem instead of masquerading
/// as a missing parameter; absent stays "required".
#[test]
fn memory_feedback_wrong_typed_useful_names_type() {
    let mut m = Map::new();
    m.insert("id".into(), json!("abc"));
    m.insert("useful".into(), json!("yes"));
    let err = parse_memory_feedback(&m).unwrap_err();
    assert!(
        err.to_string().contains("must be a boolean"),
        "wrong type must name the type, got: {err}"
    );
    let mut m = Map::new();
    m.insert("id".into(), json!("abc"));
    let err = parse_memory_feedback(&m).unwrap_err();
    assert!(
        err.to_string().contains("is required"),
        "absent must stay required, got: {err}"
    );
}

#[test]
fn memory_relate_rejects_bad_type() {
    let mut m = Map::new();
    m.insert("sourceId".into(), json!("a"));
    m.insert("targetId".into(), json!("b"));
    m.insert("type".into(), json!("bogus"));
    assert!(route_tool("memory_relate", &args(m)).is_err());
}

#[test]
fn semantic_search_requires_query() {
    assert!(route_tool("semantic_search", &None).is_err());
}

#[test]
fn unknown_tool_rejected() {
    assert!(route_tool("nonexistent", &None).is_err());
}

/// Project arguments normalize at the parse boundary (trim, basename,
/// lowercase): reads and writes meet on the canonical form.
#[test]
fn project_args_normalize_at_parse() {
    let mut args = serde_json::Map::new();
    args.insert(
        "project".to_string(),
        serde_json::Value::String("  MyProj/ ".into()),
    );
    args.insert("query".to_string(), serde_json::Value::String("x".into()));
    let req = route_tool("memory_read", &Some(args)).unwrap();
    match req {
        DomainRequest::ToolCall { tool } => match tool {
            ToolArgs::MemoryRead(parsed) => assert_eq!(
                parsed.project,
                Some("myproj".to_string()),
                "parse must normalize"
            ),
            _ => panic!("wrong tool routed"),
        },
        _ => panic!("expected tool call"),
    }
}

/// Unknown argument keys fail instead of silently dropping (a typo'd
/// filter must never become "no filter").
#[test]
fn unknown_argument_keys_rejected() {
    let mut args = serde_json::Map::new();
    args.insert(
        "fragment".to_string(),
        serde_json::Value::String("x".into()),
    );
    args.insert(
        "fragmant".to_string(),
        serde_json::Value::String("typo".into()),
    );
    let err = route_tool("memory_add", &Some(args)).unwrap_err();
    assert!(
        err.to_string().contains("fragmant"),
        "must name the unknown key, got: {err}"
    );
    // Native tools are covered too.
    let mut args = serde_json::Map::new();
    args.insert(
        "path".to_string(),
        serde_json::Value::String("/tmp/x".into()),
    );
    args.insert("bogus".to_string(), serde_json::Value::Bool(true));
    let err = route_tool("backup_preview", &Some(args)).unwrap_err();
    assert!(
        err.to_string().contains("bogus"),
        "must name the unknown key, got: {err}"
    );
}

/// The allowlist covers every served tool (no silent gaps) and matches
/// the frozen schemas exactly (no drift between validation and docs).
#[test]
fn argument_allowlist_matches_served_schemas() {
    for tool in LtmrsFrontend::tools() {
        let allowed = allowed_arguments(&tool.name)
            .unwrap_or_else(|| panic!("tool {} has no argument allowlist", tool.name));
        let schema_props: Vec<String> = tool
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let mut allowed_sorted = allowed.clone();
        allowed_sorted.sort();
        let mut schema_sorted = schema_props.clone();
        schema_sorted.sort();
        assert_eq!(
            allowed_sorted, schema_sorted,
            "allowlist drift for tool {}",
            tool.name
        );
    }
}

/// Wrong-typed values fail instead of coercing to defaults (a string
/// limit must never become "unbounded").
#[test]
fn wrong_typed_values_rejected() {
    let mut args = serde_json::Map::new();
    args.insert("limit".to_string(), serde_json::Value::String("10".into()));
    let err = route_tool("memory_read", &Some(args)).unwrap_err();
    assert!(err.to_string().contains("limit"), "got: {err}");
    let mut args = serde_json::Map::new();
    args.insert("all".to_string(), serde_json::Value::String("yes".into()));
    let err = route_tool("memory_read", &Some(args)).unwrap_err();
    assert!(err.to_string().contains("all"), "got: {err}");
    // Explicit nulls stay absent (lenient), not errors.
    let mut args = serde_json::Map::new();
    args.insert("limit".to_string(), serde_json::Value::Null);
    assert!(route_tool("memory_read", &Some(args)).is_ok());
}

/// String arrays reject non-string elements (no silent drops).
#[test]
fn string_array_elements_are_strict() {
    let mut args = serde_json::Map::new();
    args.insert("ids".to_string(), serde_json::json!(["a", 42]));
    let err = route_tool("memory_read", &Some(args)).unwrap_err();
    assert!(err.to_string().contains("ids"), "got: {err}");
}

/// The native backup tools route by name like every frozen tool (their
/// descriptions and schemas are ltmrs-native, marked in tools/list).
#[test]
fn native_backup_tools_route() {
    assert!(route_tool("backup_create", &None).is_ok());
    let mut with_path = serde_json::Map::new();
    with_path.insert(
        "path".to_string(),
        serde_json::Value::String("/tmp/x.ltmrs-backup".to_string()),
    );
    assert!(route_tool("backup_preview", &Some(with_path)).is_ok());
    assert!(route_tool("backup_restore", &None).is_ok());
}

/// Tool routing depends only on the tool name: every frozen
/// compatibility name resolves without consulting any configured server
/// name. With no arguments the outcome is either `Ok` or an
/// argument-validation error — never "unknown tool". (Hosts configure
/// their own server name/namespaces; see `skills::hosts`.)
#[test]
fn routing_needs_no_server_name() {
    let tools = frozen_tools();
    assert!(!tools.is_empty(), "frozen set must not be empty");
    for tool in &tools {
        match route_tool(&tool.name, &None) {
            Ok(_) => {}
            Err(e) => assert!(
                !e.to_string().contains("unknown tool"),
                "{} must resolve by name alone, got: {e}",
                tool.name
            ),
        }
    }
}

#[test]
fn envelope_carries_identity() {
    let id = FrontendIdentity::new(
        FrontendId::new(Uuid::from_u128(1)),
        ChannelId::new(Uuid::from_u128(2)),
    );
    let client = IpcClient::new(std::path::PathBuf::from("unused"));
    let fe = LtmrsFrontend::new(id.clone(), client);
    let params = CallToolRequestParams::new("memory_add")
        .with_arguments(json!({ "fragment": "x" }).as_object().unwrap().clone());
    let env = fe.build_envelope(&params, 7).unwrap();
    assert_eq!(env.frontend_id, id.frontend_id);
    assert_eq!(env.channel_id, id.channel_id);
    assert_eq!(env.protocol_version, PROTOCOL_VERSION);
    assert_eq!(env.retry_epoch, 7);
}

#[test]
fn shapes_structured_success_with_text() {
    use ltmrs_daemon::envelope::DomainPayload;
    let resp = ltmrs_daemon::envelope::IpcResponse::success(
        OperationId::new(Uuid::from_u128(1)),
        ltmrs_domain::command::ReceiptOutcome::Success { affected: vec![] },
        DomainPayload::ToolResult {
            text: "## Stats".into(),
            structured: Some(json!({"total": 4})),
            is_error: false,
        },
    );
    let result = shape_result(&resp);
    assert!(result.structured_content.is_some());
    assert_eq!(
        result.content[0].as_text().map(|t| t.text.as_str()),
        Some("## Stats")
    );
}

#[test]
fn shapes_tool_error() {
    use ltmrs_daemon::envelope::DomainPayload;
    let resp = ltmrs_daemon::envelope::IpcResponse::success(
        OperationId::new(Uuid::from_u128(1)),
        ltmrs_domain::command::ReceiptOutcome::Success { affected: vec![] },
        DomainPayload::ToolResult {
            text: "Error: Fragment not found".into(),
            structured: None,
            is_error: true,
        },
    );
    let result = shape_result(&resp);
    assert_eq!(result.is_error, Some(true));
}
