//! Slice A A3: daemon-side Hindsight bridge wiring.
//!
//! The dispatcher holds an optional [`BridgeHandle`]; `execute_tool` calls
//! [`Dispatcher::enqueue_bridge_retain`] after a successful mutating tool.
//! Best-effort only: disabled (default) or full-queue or down all degrade
//! to local-only with no tool failure, no receipt change, no extra
//! invalidation beyond the existing write invalidation.

use ltmrs_compat::lemma::tool_args::ToolArgs;
use ltmrs_service::projects::ResolvedScope;

/// Build a [`RetainDoc`] for tools in the fan-out set; `None` for everything
/// else (reads, stats, backup, session bookkeeping, forgets-as-deletes).
pub fn retain_doc_for(
    tool: &ToolArgs,
    payload_text: &str,
) -> Option<ltmrs_interchange::hindsight::RetainDoc> {
    use ltmrs_compat::lemma::tool_args::ToolArgs as T;
    use ltmrs_domain::id::EntityId;
    let (collection, text) = match tool {
        T::MemoryAdd(a) => ("memory", a.fragment.clone()),
        T::MemoryUpdate(a) => ("memory", a.fragment.clone().unwrap_or_default()),
        T::MemoryFeedback(_) => ("memory", payload_text.to_string()),
        T::MemoryMerge(a) => ("memory", a.fragment.clone()),
        T::MemoryRelate(_) => ("memory", payload_text.to_string()),
        T::GuideCreate(a) => (
            "guide",
            a.learnings.join(
                "
",
            ),
        ),
        T::GuideUpdate(a) => ("guide", a.description.clone().unwrap_or_default()),
        T::GuideDistill(_) => ("guide", payload_text.to_string()),
        T::SessionEnd(a) => ("session", a.final_approach.clone().unwrap_or_default()),
        _ => return None,
    };
    if text.trim().is_empty() {
        return None;
    }
    // Every committed mutation is a distinct event: mint a fresh id per
    // call (deterministic ids belong to canonical rows, not bridge docs).
    let eid = EntityId::new(uuid::Uuid::now_v7());
    Some(ltmrs_interchange::hindsight::RetainDoc {
        document_id: ltmrs_interchange::hindsight::document_id(collection, &eid),
        bank: String::new(),
        tags: Vec::new(),
        title: String::new(),
        text,
        project: None,
    })
}

/// Scope tags for one retained doc: the resolved stable name/legacy
/// (pure; never probes). Handle defaults merge at enqueue time.
pub fn tags_for_retain(resolved: &ResolvedScope) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(n) = resolved.name.as_deref().filter(|s| !s.is_empty()) {
        tags.push(format!("project:{n}"));
    } else if let Some(l) = resolved.legacy.as_deref().filter(|s| !s.is_empty()) {
        tags.push(format!("project:{l}"));
    }
    tags
}

/// Bridge message: fire-and-forget retains + request/response recalls.
enum BridgeMsg {
    Retain(ltmrs_interchange::hindsight::RetainDoc),
    Recall {
        query: String,
        bank: String,
        tags: Vec<String>,
        max_tokens: u32,
        reply: tokio::sync::oneshot::Sender<Vec<ltmrs_interchange::hindsight::RecallHit>>,
    },
    Reflect {
        query: String,
        bank: String,
        tags: Vec<String>,
        reply: tokio::sync::oneshot::Sender<String>,
    },
}

/// Bounded background sender + recall round-trip (Slice A A2/A4, G1a scoping).
/// Never blocks the caller: retains drop-newest; recalls time out.
pub struct BridgeHandle {
    tx: tokio::sync::mpsc::Sender<BridgeMsg>,
    bank: String,
    default_tags: Vec<String>,
    scoping: ltmrs_interchange::hindsight::Scoping,
}

impl BridgeHandle {
    pub fn spawn<T>(
        client: ltmrs_interchange::hindsight::client::HindsightClient<T>,
        bank: String,
        tags: Vec<String>,
        max_queue: usize,
    ) -> (Self, tokio::task::JoinHandle<()>)
    where
        T: ltmrs_interchange::hindsight::client::HttpTransport + 'static,
    {
        Self::spawn_scoped(
            client,
            bank,
            tags,
            max_queue,
            ltmrs_interchange::hindsight::Scoping::PerProjectTagged,
        )
    }

    /// Scoped spawn (G1a): bank + recall tag filter follow the scoping mode.
    pub fn spawn_scoped<T>(
        client: ltmrs_interchange::hindsight::client::HindsightClient<T>,
        bank: String,
        tags: Vec<String>,
        max_queue: usize,
        scoping: ltmrs_interchange::hindsight::Scoping,
    ) -> (Self, tokio::task::JoinHandle<()>)
    where
        T: ltmrs_interchange::hindsight::client::HttpTransport + 'static,
    {
        Self::spawn_scoped_outbox(client, bank, tags, max_queue, scoping, None)
    }

    /// Scoped spawn with durable outbox (G1d): retains persist to disk
    /// before send; failures retry with backoff (5s → 30m, ±jitter omitted —
    /// deterministic capped backoff keeps tests honest) and stay on disk.
    pub fn spawn_scoped_outbox<T>(
        client: ltmrs_interchange::hindsight::client::HindsightClient<T>,
        bank: String,
        tags: Vec<String>,
        max_queue: usize,
        scoping: ltmrs_interchange::hindsight::Scoping,
        outbox: Option<std::path::PathBuf>,
    ) -> (Self, tokio::task::JoinHandle<()>)
    where
        T: ltmrs_interchange::hindsight::client::HttpTransport + 'static,
    {
        use ltmrs_interchange::hindsight::outbox as ob;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMsg>(max_queue.max(1));
        let handle = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    BridgeMsg::Retain(doc) => {
                        // Persist first when an outbox root is configured.
                        let event_path = outbox.as_ref().and_then(|root| {
                            let dir = ob::dest_dir(root, &doc.bank);
                            let body = serde_json::json!({
                                "document_id": doc.document_id,
                                "bank": doc.bank,
                                "tags": doc.tags,
                                "title": doc.title,
                                "text": doc.text,
                            });
                            let bytes = serde_json::to_vec(&body).ok()?;
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos() as u64)
                                .unwrap_or(0);
                            ob::write_event(
                                &dir,
                                &ob::event_filename(&doc.document_id, now),
                                &bytes,
                            )
                            .ok()
                        });
                        // Bounded retry: 5s, 30s, 5m, then leave the file.
                        let mut attempt = 0u32;
                        loop {
                            match client.retain(&doc, &doc.tags).await {
                                Ok(()) => {
                                    if let Some(p) = event_path.as_ref() {
                                        let _ = std::fs::remove_file(p);
                                    }
                                    break;
                                }
                                Err(_) if attempt < 3 => {
                                    attempt += 1;
                                    let backoff = [5u64, 30, 300][attempt as usize - 1];
                                    tokio::time::sleep(std::time::Duration::from_secs(backoff))
                                        .await;
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    BridgeMsg::Recall {
                        query,
                        bank,
                        tags,
                        max_tokens,
                        reply,
                    } => {
                        let hits = client
                            .recall(&bank, &query, &tags, max_tokens)
                            .await
                            .unwrap_or_default();
                        let _ = reply.send(hits);
                    }
                    BridgeMsg::Reflect {
                        query,
                        bank,
                        tags,
                        reply,
                    } => {
                        let text = client
                            .reflect(&bank, &query, &tags)
                            .await
                            .unwrap_or_default();
                        let _ = reply.send(text);
                    }
                }
            }
        });
        (
            Self {
                tx,
                bank,
                default_tags: tags,
                scoping,
            },
            handle,
        )
    }

    /// Enqueue a retain (best-effort: full queue drops newest + counts).
    /// Stamps the handle's bank and merges default tags first.
    pub fn enqueue_retain(&self, mut doc: ltmrs_interchange::hindsight::RetainDoc) -> bool {
        if doc.bank.is_empty() {
            doc.bank.clone_from(&self.bank);
        }
        let mut tags = self.default_tags.clone();
        tags.append(&mut doc.tags);
        tags.sort();
        tags.dedup();
        doc.tags = tags;
        self.tx.try_send(BridgeMsg::Retain(doc)).is_ok()
    }

    /// Bank for a project scope (G1a): shared base for global/tagged,
    /// base + label for per-project. Pure derivation, no I/O.
    pub fn bank_for_project(&self, project: Option<&str>) -> String {
        ltmrs_interchange::hindsight::bank_for_scope(
            &ltmrs_interchange::hindsight::HindsightConfig {
                bank: self.bank.clone(),
                ..ltmrs_interchange::hindsight::HindsightConfig::default()
            },
            &self.scoping,
            project,
        )
    }

    /// Recall tag filter for a project scope (G1a): tagged mode filters on
    /// the label, global/per-project read the bank itself. Pure.
    pub fn recall_tags_for(&self, project: Option<&str>) -> Vec<String> {
        ltmrs_interchange::hindsight::tags_for_recall(&self.scoping, project)
    }
    /// Scoped recall with fusion (A4): canonical-first, Hindsight appended.
    /// Best-effort: unconfigured, unreachable, or timed-out all yield "".
    /// Sync-safe: uses `block_in_place` so sync tool dispatch never parks
    /// the runtime; without a runtime (plain unit tests) degrades to "".
    pub fn recall_section(
        &self,
        query: &str,
        bank: &str,
        tags: &[String],
        canonical_text: &str,
        max_tokens: u32,
        timeout_ms: u64,
    ) -> String {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let msg = BridgeMsg::Recall {
            query: query.to_string(),
            bank: if bank.is_empty() {
                self.bank.clone()
            } else {
                bank.to_string()
            },
            tags: {
                let mut t = self.default_tags.clone();
                t.extend(tags.iter().cloned());
                t.sort();
                t.dedup();
                t
            },
            max_tokens,
            reply: reply_tx,
        };
        if self.tx.try_send(msg).is_err() {
            return String::new();
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return String::new();
        };
        let hits = tokio::task::block_in_place(|| {
            rt.block_on(async move {
                tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms.max(1)),
                    reply_rx,
                )
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default()
            })
        });
        ltmrs_interchange::hindsight::merge_hindsight_hits(canonical_text, &hits)
    }

    /// Synchronous bridge reflect (G1b): server-side synthesis over the
    /// bank, best-effort ("" when disabled/unreachable/timed-out/empty).
    /// Sync-safe like recall_section; blank server replies degrade to "".
    pub fn reflect(&self, query: &str, bank: &str, tags: &[String], timeout_ms: u64) -> String {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let mut t = self.default_tags.clone();
        t.extend(tags.iter().cloned());
        t.sort();
        t.dedup();
        let msg = BridgeMsg::Reflect {
            query: query.to_string(),
            bank: if bank.is_empty() {
                self.bank.clone()
            } else {
                bank.to_string()
            },
            tags: t,
            reply: reply_tx,
        };
        if self.tx.try_send(msg).is_err() {
            return String::new();
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return String::new();
        };
        tokio::task::block_in_place(|| {
            rt.block_on(async move {
                tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms.max(1)),
                    reply_rx,
                )
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltmrs_compat::lemma::tool_args::{MemoryAddArgs, ToolArgs};
    use ltmrs_service::projects::ResolvedScope;

    /// A3 RED: disabled bridge (no handle) is a no-op.
    #[test]
    fn disabled_bridge_is_noop() {
        use crate::dispatcher::Dispatcher;
        use crate::registry::FrontendRegistry;
        use ltmrs_domain::clock::FrozenClock;
        use std::sync::Arc;
        let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let dir = tempfile::tempdir().unwrap();
        let repo = Arc::new(
            ltmrs_service::repository::CanonicalRepository::open_with_clock(
                dir.path().to_str().unwrap(),
                Arc::clone(&clock),
            )
            .unwrap(),
        );
        std::mem::forget(dir);
        let disp = Dispatcher::new(repo, FrontendRegistry::new(), clock);
        let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Hi\n\n### Context\nX.".to_string(),
            title: None,
            description: None,
            project: None,
            source: None,
            confirm: false,
            fragment_type: None,
            evidence: None,
        });
        assert!(!disp.enqueue_bridge_retain(
            &tool,
            "text",
            &ResolvedScope::unattributed(),
            ltmrs_domain::id::OperationId::new(uuid::Uuid::from_u128(1))
        ));
    }

    /// P0 (review): a redacted add must forward the STORED (redacted)
    /// fragment, never the raw request args. RED: fails — the bridge
    /// forwards `ToolArgs` verbatim (`bridge_doc_for_test` does not exist).
    #[test]
    fn bridge_forwards_stored_redacted_fragment() {
        use crate::dispatcher::Dispatcher;
        use crate::registry::FrontendRegistry;
        use ltmrs_domain::clock::FrozenClock;
        use ltmrs_domain::command::DomainCommand;
        use std::sync::Arc;
        let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let dir = tempfile::tempdir().unwrap();
        let repo = Arc::new(
            ltmrs_service::repository::CanonicalRepository::open_with_clock(
                dir.path().to_str().unwrap(),
                Arc::clone(&clock),
            )
            .unwrap(),
        );
        std::mem::forget(dir);
        // Commit the REDACTED form directly under the deterministic
        // entity id the add path derives from the operation id.
        let op = ltmrs_domain::id::OperationId::new(uuid::Uuid::from_u128(77));
        let eid = ltmrs_domain::id::EntityId::new(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("ltmrs:entity:{}", op.as_uuid()).as_bytes(),
        ));
        let fe = ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1));
        let ch = ltmrs_domain::id::ChannelId::new(uuid::Uuid::from_u128(2));
        repo.issue_namespace(fe, ch, 1000).unwrap();
        // (Memory struct below carries the committed REDACTED form.)
        let memory = ltmrs_domain::memory::Memory {
            id: eid,
            external_alias: None,
            title: "t".into(),
            fragment: "[REDACTED]".into(),
            description: String::new(),
            fragment_type: ltmrs_domain::memory::FragmentType::Fact,
            project: None,
            project_id: None,
            source: ltmrs_domain::memory::MemorySource::Ai,
            confidence: 1.0,
            quality_score: None,
            lifecycle: ltmrs_domain::memory::MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: ltmrs_domain::id::EntityRevision::new(1),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(1),
            created_at: ltmrs_domain::memory::Instant::new(1000),
            updated_at: ltmrs_domain::memory::Instant::new(1000),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        };
        let ctx = ltmrs_domain::command::CommandContext {
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            frontend_id: fe,
            channel_id: ch,
            retry_epoch: 1,
            operation_id: op,
            request_digest: "d".into(),
            session: None,
            deadline_millis: None,
            scope: Default::default(),
        };
        repo.apply(
            &ctx,
            &DomainCommand::AddMemory {
                memory,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
        let disp = Dispatcher::new(repo, FrontendRegistry::new(), clock);
        // Raw request args (pre-redaction) must NOT reach the bridge.
        let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "sk-live-abc123".to_string(),
            title: None,
            description: None,
            project: None,
            source: None,
            confirm: false,
            fragment_type: None,
            evidence: None,
        });
        let doc = disp
            .bridge_doc_for(&tool, op, "")
            .expect("stored memory must resolve");
        assert!(
            !doc.text.contains("sk-live-abc123"),
            "bridge must not forward the raw secret, got: {}",
            doc.text
        );
        assert!(
            doc.text.contains("[REDACTED]"),
            "bridge must forward the stored text, got: {}",
            doc.text
        );
    }

    /// A3: fan-out set maps to docs; reads map to None.
    #[test]
    fn fanout_set_maps_reads_do_not() {
        let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Hi\n\n### Context\nX.".to_string(),
            title: None,
            description: None,
            project: None,
            source: None,
            confirm: false,
            fragment_type: None,
            evidence: None,
        });
        let doc = retain_doc_for(&tool, "").unwrap();
        assert!(doc.document_id.starts_with("ltmrs:memory:"));
        let read = ToolArgs::MemoryStats(Default::default());
        assert!(retain_doc_for(&read, "").is_none());
        let forget = ToolArgs::MemoryForget(Default::default());
        assert!(retain_doc_for(&forget, "").is_none());
    }

    /// A3: mock handle receives one stamped retain per fanned tool.
    /// multi_thread: the test polls for background delivery, so the worker
    /// needs its own thread (a current-thread runtime would park it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_handle_receives_stamped_retain() {
        use ltmrs_interchange::hindsight::client::{HindsightClient, HttpTransport};
        use ltmrs_interchange::hindsight::{BridgeError, HindsightConfig};
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Mock {
            pub calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        }
        impl HttpTransport for Mock {
            async fn post(
                &self,
                url: &str,
                _bearer: Option<&str>,
                body: serde_json::Value,
            ) -> Result<serde_json::Value, BridgeError> {
                self.calls.lock().unwrap().push((url.to_string(), body));
                Ok(serde_json::json!({"ok": true}))
            }
        }

        let mock = Mock::default();
        let calls = Arc::clone(&mock.calls);
        let cfg = HindsightConfig {
            enabled: true,
            base_url: "http://127.0.0.1:9999".into(),
            bank: "team".into(),
            default_tags: vec!["t".into()],
            timeout_ms: 1500,
            max_queue: 8,
            scoping: ltmrs_interchange::hindsight::Scoping::PerProjectTagged,
            global_scope: false,
            outbox_dir: None,
        };
        let client = HindsightClient::with_transport(cfg, None, mock);
        let (handle, _bg) = BridgeHandle::spawn(client, "team".into(), vec!["t".into()], 8);
        let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Hi\n\n### Context\nX.".to_string(),
            title: None,
            description: None,
            project: None,
            source: None,
            confirm: false,
            fragment_type: None,
            evidence: None,
        });
        use crate::dispatcher::Dispatcher;
        use crate::registry::FrontendRegistry;
        use ltmrs_domain::clock::FrozenClock;
        use std::sync::Arc as StdArc;
        let clock: StdArc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
            StdArc::new(FrozenClock::new(1000));
        let dir = tempfile::tempdir().unwrap();
        let repo = StdArc::new(
            ltmrs_service::repository::CanonicalRepository::open_with_clock(
                dir.path().to_str().unwrap(),
                StdArc::clone(&clock),
            )
            .unwrap(),
        );
        std::mem::forget(dir);
        let disp = Dispatcher::new(repo, FrontendRegistry::new(), clock).with_hindsight(handle);
        assert!(disp.enqueue_bridge_retain(
            &tool,
            "",
            &ResolvedScope::unattributed(),
            ltmrs_domain::id::OperationId::new(uuid::Uuid::from_u128(2))
        ));
        // Background worker delivers: poll briefly (no sleeps in prod paths;
        // tests may poll with a deadline).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let guard = calls.lock().unwrap();
                if !guard.is_empty() {
                    let (url, body) = &guard[0];
                    assert!(
                        url.ends_with("/v1/default/banks/team/memories"),
                        "got: {url}"
                    );
                    let items = body.get("items").and_then(|i| i.as_array()).unwrap();
                    assert_eq!(items.len(), 1);
                    let doc_id = items[0]
                        .get("document_id")
                        .and_then(|d| d.as_str())
                        .unwrap();
                    assert!(doc_id.starts_with("ltmrs:memory:"), "got: {doc_id}");
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "bridge worker never delivered"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// P0b (review): per-project retain uses the project bank, not the
    /// shared handle default. Mock transport captures the request URL —
    /// the bank rides the path, so the URL pins the derivation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retain_bank_follows_resolved_project() {
        use crate::dispatcher::Dispatcher;
        use crate::registry::FrontendRegistry;
        use ltmrs_domain::clock::FrozenClock;
        use ltmrs_interchange::hindsight::client::{HindsightClient, HttpTransport};
        use ltmrs_interchange::hindsight::{BridgeError, HindsightConfig, Scoping};
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Mock {
            pub calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        }
        impl HttpTransport for Mock {
            async fn post(
                &self,
                url: &str,
                _bearer: Option<&str>,
                body: serde_json::Value,
            ) -> Result<serde_json::Value, BridgeError> {
                self.calls.lock().unwrap().push((url.to_string(), body));
                Ok(serde_json::json!({"ok": true}))
            }
        }

        let mock = Mock::default();
        let calls = Arc::clone(&mock.calls);
        let cfg = HindsightConfig {
            enabled: true,
            base_url: "http://127.0.0.1:9999".into(),
            bank: "team".into(),
            default_tags: vec![],
            timeout_ms: 1500,
            max_queue: 8,
            scoping: Scoping::PerProject,
            global_scope: false,
            outbox_dir: None,
        };
        let client = HindsightClient::with_transport(cfg, None, mock);
        let (handle, _bg) = super::BridgeHandle::spawn_scoped(
            client,
            "team".into(),
            vec![],
            8,
            Scoping::PerProject,
        );
        let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let dir = tempfile::tempdir().unwrap();
        let repo = Arc::new(
            ltmrs_service::repository::CanonicalRepository::open_with_clock(
                dir.path().to_str().unwrap(),
                Arc::clone(&clock),
            )
            .unwrap(),
        );
        std::mem::forget(dir);
        let disp = Dispatcher::new(repo, FrontendRegistry::new(), clock).with_hindsight(handle);
        let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "plain text".to_string(),
            title: None,
            description: None,
            project: None,
            source: None,
            confirm: false,
            fragment_type: None,
            evidence: None,
        });
        let resolved = ResolvedScope {
            name: Some("proj".into()),
            ..ResolvedScope::unattributed()
        };
        assert!(disp.enqueue_bridge_retain(
            &tool,
            "",
            &resolved,
            ltmrs_domain::id::OperationId::new(uuid::Uuid::from_u128(9))
        ));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let guard = calls.lock().unwrap();
                if !guard.is_empty() {
                    let (url, _) = &guard[0];
                    assert!(
                        url.ends_with("/v1/default/banks/team-proj/memories"),
                        "retain must target the project bank, got: {url}"
                    );
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "bridge worker never delivered"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
