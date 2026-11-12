//! Slice A A2: Hindsight HTTP client (mockable) + background sender.
//!
//! The prod path uses `reqwest` (blocking within `spawn_blocking`, never on
//! the tool ACK path — see placement decision). Tests inject a mock
//! [`HttpTransport`]; no test touches the network.

use super::types::{BridgeError, HindsightConfig, RecallHit, RetainDoc};
use super::{recall_path, reflect_path, retain_path};

/// Minimal HTTP surface the bridge needs (mockable; prod = reqwest).
/// Native async-trait methods (Rust 2024, no async-trait crate needed).
pub trait HttpTransport: Send + Sync {
    fn post(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: serde_json::Value,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, BridgeError>> + Send;
}

/// Reqwest-backed transport (the only HTTP-touching code in the bridge).
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport for ReqwestTransport {
    async fn post(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, BridgeError> {
        let mut req = self.client.post(url).json(&body);
        if let Some(token) = bearer.filter(|t| !t.is_empty()) {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                BridgeError::Unavailable(format!("timeout: {e}"))
            } else {
                BridgeError::Unavailable(format!("transport: {e}"))
            }
        })?;
        if !resp.status().is_success() {
            return Err(BridgeError::Rejected(format!("HTTP {}", resp.status())));
        }
        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| BridgeError::Rejected(format!("bad JSON: {e}")))
    }
}

/// The bridge client: config + transport. All methods take an explicit
/// per-call timeout (callers pass `config.timeout_ms`).
pub struct HindsightClient<T: HttpTransport = ReqwestTransport> {
    config: HindsightConfig,
    bearer: Option<String>,
    transport: T,
}

impl HindsightClient<ReqwestTransport> {
    pub fn new(config: HindsightConfig, bearer: Option<String>) -> Self {
        Self {
            config,
            bearer,
            transport: ReqwestTransport::new(),
        }
    }
}

impl<T: HttpTransport> HindsightClient<T> {
    pub fn with_transport(config: HindsightConfig, bearer: Option<String>, transport: T) -> Self {
        Self {
            config,
            bearer,
            transport,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.base_url.trim_end_matches('/'), path)
    }

    /// Retain one document (idempotent upsert via stable `document_id`).
    pub async fn retain(&self, doc: &RetainDoc, tags: &[String]) -> Result<(), BridgeError> {
        if !self.config.enabled {
            return Err(BridgeError::Disabled);
        }
        let body = serde_json::json!({
            "items": [{
                "content": doc.text,
                "context": doc.title,
                "document_id": doc.document_id,
                "tags": tags,
                "metadata": {"ltmrs_collection": doc.bank, "ltmrs_project": doc.project},
            }],
        });
        let url = self.url(&retain_path(&doc.bank));
        self.transport
            .post(&url, self.bearer.as_deref(), body)
            .await
            .map(|_| ())
    }

    /// Recall: scoped query, token-budgeted server-side.
    pub async fn recall(
        &self,
        bank: &str,
        query: &str,
        tags: &[String],
        max_tokens: u32,
    ) -> Result<Vec<RecallHit>, BridgeError> {
        if !self.config.enabled {
            return Err(BridgeError::Disabled);
        }
        let body = serde_json::json!({
            "query": query,
            "max_tokens": max_tokens,
            "budget": "mid",
            "tags": tags,
            "tags_match": "any",
        });
        let url = self.url(&recall_path(bank));
        let v = self
            .transport
            .post(&url, self.bearer.as_deref(), body)
            .await?;
        Ok(v.get("results")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|r| {
                Some(RecallHit {
                    document_id: r
                        .get("id")
                        .or_else(|| r.get("document_id"))?
                        .as_str()?
                        .to_string(),
                    score: r.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0),
                    snippet: r
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
            })
            .collect())
    }

    /// Reflect (manual-only in B1): agentic synthesis over the bank.
    pub async fn reflect(
        &self,
        bank: &str,
        query: &str,
        tags: &[String],
    ) -> Result<String, BridgeError> {
        if !self.config.enabled {
            return Err(BridgeError::Disabled);
        }
        let body = serde_json::json!({
            "query": query,
            "budget": "low",
            "tags": tags,
            "tags_match": "any",
        });
        let url = self.url(&reflect_path(bank));
        let v = self
            .transport
            .post(&url, self.bearer.as_deref(), body)
            .await?;
        Ok(v.get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hindsight::types::HindsightConfig;
    use std::sync::Mutex;

    /// In-process mock transport: records requests, replays canned JSON.
    struct Mock {
        pub calls: Mutex<Vec<(String, serde_json::Value)>>,
        pub reply: serde_json::Value,
        pub fail: Option<String>,
    }

    impl Mock {
        fn new(reply: serde_json::Value) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                reply,
                fail: None,
            }
        }
    }

    impl HttpTransport for Mock {
        async fn post(
            &self,
            url: &str,
            _bearer: Option<&str>,
            body: serde_json::Value,
        ) -> Result<serde_json::Value, BridgeError> {
            self.calls.lock().unwrap().push((url.to_string(), body));
            if let Some(e) = &self.fail {
                return Err(BridgeError::Unavailable(e.clone()));
            }
            Ok(self.reply.clone())
        }
    }

    fn cfg() -> HindsightConfig {
        HindsightConfig {
            enabled: true,
            base_url: "http://127.0.0.1:9999".into(),
            bank: "team".into(),
            default_tags: vec![],
            timeout_ms: 1500,
            max_queue: 8,
        }
    }

    /// A2: retain posts an upsert body to the precedent path.
    #[tokio::test]
    async fn retain_posts_upsert_body() {
        let mock = Mock::new(serde_json::json!({"ok": true}));
        let client = HindsightClient::with_transport(cfg(), None, mock);
        let doc = RetainDoc {
            document_id: "ltmrs:memory:abc".into(),
            bank: "team".into(),
            tags: vec!["project:shop".into()],
            title: "T".into(),
            text: "body".into(),
            project: Some("shop".into()),
        };
        client
            .retain(&doc, &["project:shop".to_string()])
            .await
            .unwrap();
    }

    /// A2: transport failure maps to Unavailable (fail-open upstream).
    #[tokio::test]
    async fn transport_failure_is_unavailable() {
        let mut mock = Mock::new(serde_json::json!({}));
        mock.fail = Some("down".into());
        let client = HindsightClient::with_transport(cfg(), None, mock);
        let doc = RetainDoc {
            document_id: "x".into(),
            bank: "team".into(),
            tags: vec![],
            title: "".into(),
            text: "".into(),
            project: None,
        };
        assert_eq!(
            client.retain(&doc, &[]).await,
            Err(BridgeError::Unavailable("down".into()))
        );
    }

    /// A2: recall parses results, tolerates missing fields.
    #[tokio::test]
    async fn recall_parses_results() {
        let mock = Mock::new(serde_json::json!({
            "results": [
                {"id": "ltmrs:memory:a", "score": 0.9, "text": "snip"},
                {"nope": true}
            ]
        }));
        let client = HindsightClient::with_transport(cfg(), None, mock);
        let hits = client.recall("team", "q", &[], 100).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].document_id, "ltmrs:memory:a");
    }
}
