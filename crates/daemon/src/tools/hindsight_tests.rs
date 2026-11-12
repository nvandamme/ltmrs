use super::test_support::*;
use ltmrs_compat::lemma::tool_args::{SemanticSearchArgs, ToolArgs};

/// A4 RED: with a mock bridge, semantic_search appends the Hindsight
/// section; canonical order untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_appends_hindsight_section() {
    use crate::hindsight::BridgeHandle;
    use ltmrs_interchange::hindsight::client::{HindsightClient, HttpTransport};
    use ltmrs_interchange::hindsight::{BridgeError, HindsightConfig};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Mock {
        calls: Arc<Mutex<usize>>,
    }
    impl HttpTransport for Mock {
        async fn post(
            &self,
            _url: &str,
            _bearer: Option<&str>,
            _body: serde_json::Value,
        ) -> Result<serde_json::Value, BridgeError> {
            *self.calls.lock().unwrap() += 1;
            Ok(serde_json::json!({
                "results": [{"id": "hs:1", "score": 0.88, "text": "hindsight-only insight"}]
            }))
        }
    }

    let (disp, _dir) = test_dispatcher();
    let cfg = HindsightConfig {
        enabled: true,
        base_url: "http://127.0.0.1:9999".into(),
        bank: "team".into(),
        default_tags: vec![],
        timeout_ms: 1500,
        max_queue: 8,
    };
    let client = HindsightClient::with_transport(cfg, None, Mock::default());
    let (handle, _bg) = BridgeHandle::spawn(client, "team".into(), vec![], 8);
    let disp = disp.with_hindsight(handle);

    // Seed one canonical memory, then search for it.
    let seed = ToolArgs::MemoryAdd(ltmrs_compat::lemma::tool_args::MemoryAddArgs {
        fragment: "## Canonical Shop\n\n### Context\nLocal durable fact.".to_string(),
        title: Some("Canonical Shop".to_string()),
        description: None,
        project: None,
        source: None,
        confirm: true,
        fragment_type: None,
        evidence: None,
    });
    let id = ident(None, None, None, false);
    let out = run(&disp, &tool_call_with(1, seed.clone(), id.clone()), &seed);
    assert!(!result_is_error(&out), "seed failed: {}", result_text(&out));

    let tool = ToolArgs::SemanticSearch(SemanticSearchArgs {
        query: "shop".to_string(),
        project: None,
        top_k: Some(5),
        offset: None,
        hybrid: Some(false),
        explain: false,
        response_format: None,
    });
    let resp = run(&disp, &tool_call_with(2, tool.clone(), id), &tool);
    assert!(
        !result_is_error(&resp),
        "search failed: {}",
        result_text(&resp)
    );
    let text = result_text(&resp);
    assert!(
        text.contains("Canonical Shop"),
        "canonical first, got: {text}"
    );
    assert!(
        text.contains("Hindsight suggestions"),
        "fused section must append, got: {text}"
    );
    assert!(
        text.contains("hindsight-only insight"),
        "hit snippet must render, got: {text}"
    );
}
