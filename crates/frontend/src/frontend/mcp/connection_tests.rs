//! MCP connection/handshake/resume tests (moved verbatim from `mcp.rs`).

use std::sync::Arc;

use super::test_support::*;
use super::{FrontendIdentity, LtmrsFrontend};
use ltmrs_compat::lemma::tool_args::{MemoryAddArgs, ToolArgs};
use ltmrs_daemon::client::IpcClient;
use ltmrs_daemon::envelope::{DomainRequest, IpcResult};
use ltmrs_domain::id::{ChannelId, FrontendId};
/// Trait import for the unix-socket test doubles below (method resolution
/// only); unused on Windows where those tests are gated out.
#[cfg(unix)]
use rmcp::ServerHandler;
use uuid::Uuid;

/// Post-restore regression: a frontend constructed at generation FIRST
/// must handshake against a daemon at generation 2 by adopting the live
/// generation (not brick with GenerationMismatch).
#[tokio::test]
async fn handshake_adopts_live_generation_after_restore() {
    use ltmrs_daemon::runtime::RuntimePaths;
    use ltmrs_daemon::server::connection::handle_connection;
    use ltmrs_daemon::server::{Daemon, DaemonConfig};

    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::resolve(dir.path(), "test-store");
    let config = DaemonConfig {
        store_path: dir.path().join("store").to_str().unwrap().to_string(),
        ..Default::default()
    };
    let daemon = Daemon::start(&paths, config).await.unwrap();
    // Simulate post-restore: the live generation moves to 2 while the
    // frontend still believes FIRST.
    daemon
        .dispatcher_arc()
        .repo()
        .set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
        .unwrap();
    let dispatcher = daemon.dispatcher_arc();
    let quotas = daemon.quotas();
    // Scripted wire server on its own endpoint: the live daemon above
    // already holds the store endpoint (rebinding it would be AddrInUse).
    let paths = ltmrs_daemon::runtime::RuntimePaths::resolve(dir.path(), "script-server");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    let listener = ltmrs_daemon::runtime::bind_listener(&paths).unwrap();
    let socket = paths.endpoint.clone();
    let server_handle = tokio::spawn(async move {
        loop {
            let Ok(stream) = listener.accept().await else {
                break;
            };
            let dispatcher = dispatcher.clone();
            let quotas = quotas.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, dispatcher, quotas).await;
            });
        }
    });

    let mut client = IpcClient::new(socket);
    client.connect().await.unwrap();
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        ),
        client,
    );
    fe.ensure_handshaked().await.unwrap();
    assert_eq!(
        fe.identity.generation(),
        ltmrs_domain::id::StoreGeneration::new(2),
        "frontend must adopt the live generation"
    );

    drop(daemon);
    server_handle.abort();
}

/// Established-connection regression (review Critical): after a second
/// restore bumps the live generation, an already-handshaked frontend
/// must re-handshake and retry once instead of failing StaleGeneration
/// on every subsequent call.
#[tokio::test]
async fn established_connection_rehandshakes_on_stale_generation() {
    use ltmrs_daemon::runtime::RuntimePaths;
    use ltmrs_daemon::server::connection::handle_connection;
    use ltmrs_daemon::server::{Daemon, DaemonConfig};

    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::resolve(dir.path(), "test-store");
    let config = DaemonConfig {
        store_path: dir.path().join("store").to_str().unwrap().to_string(),
        ..Default::default()
    };
    let daemon = Daemon::start(&paths, config).await.unwrap();
    daemon
        .dispatcher_arc()
        .repo()
        .set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
        .unwrap();
    let dispatcher = daemon.dispatcher_arc();
    let quotas = daemon.quotas();
    // Scripted wire server on its own endpoint: the live daemon above
    // already holds the store endpoint (rebinding it would be AddrInUse).
    let paths = ltmrs_daemon::runtime::RuntimePaths::resolve(dir.path(), "script-server");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    let listener = ltmrs_daemon::runtime::bind_listener(&paths).unwrap();
    let socket = paths.endpoint.clone();
    let server_handle = tokio::spawn(async move {
        loop {
            let Ok(stream) = listener.accept().await else {
                break;
            };
            let dispatcher = dispatcher.clone();
            let quotas = quotas.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, dispatcher, quotas).await;
            });
        }
    });

    let mut client = IpcClient::new(socket);
    client.connect().await.unwrap();
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        ),
        client,
    );
    fe.ensure_handshaked().await.unwrap();
    assert_eq!(
        fe.identity.generation(),
        ltmrs_domain::id::StoreGeneration::new(2)
    );
    // Second restore while the connection is established.
    daemon
        .dispatcher_arc()
        .repo()
        .set_store_generation(ltmrs_domain::id::StoreGeneration::new(3))
        .unwrap();
    let resp = fe
        .roundtrip_with_rehandshake(DomainRequest::ListMemories, false)
        .await
        .unwrap();
    assert!(
        matches!(
            resp.result,
            ltmrs_daemon::envelope::IpcResult::Success { .. }
        ),
        "established connection must recover, got {:?}",
        resp.result
    );
    assert_eq!(
        fe.identity.generation(),
        ltmrs_domain::id::StoreGeneration::new(3),
        "frontend must adopt the newest live generation"
    );

    drop(daemon);
    server_handle.abort();
}

/// P1-B unknown-outcome recovery, end to end: the server commits a
/// mutation then drops the connection without responding. The frontend
/// must reconnect, resume the SAME retry epoch, and resend the SAME
/// envelope — the daemon replays its receipt (a duplicate-ID AddMemory
/// would fail as "already exists" if it executed twice). Exactly one
/// memory exists afterwards. Deterministic: the drop point is
/// server-controlled, not timed.
#[tokio::test]
async fn unknown_outcome_resends_same_envelope_after_resume() {
    use ltmrs_daemon::dispatcher::Dispatcher;
    use ltmrs_daemon::registry::FrontendRegistry;
    use ltmrs_domain::clock::{Clock, FrozenClock};
    use ltmrs_service::repository::CanonicalRepository;
    use tokio::io::AsyncReadExt;

    async fn read_msg(
        stream: &mut ltmrs_daemon::runtime::IpcStream,
    ) -> ltmrs_daemon::envelope::WireMessage {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await.unwrap();
        serde_json::from_slice(&buf).unwrap()
    }
    async fn write_msg(
        stream: &mut ltmrs_daemon::runtime::IpcStream,
        reply: &ltmrs_daemon::envelope::WireReply,
    ) {
        // Same streaming reply format as the production server
        // (8-byte total + chunk frames), so the client parses it.
        let payload = serde_json::to_vec(reply).unwrap();
        ltmrs_daemon::envelope::write_response_payload(stream, &payload)
            .await
            .unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(
            dir.path().join("store").to_str().unwrap(),
            Arc::clone(&clock),
        )
        .unwrap(),
    );
    // Epoch 1 will be issued by the first handshake below; the
    // frontend must resume exactly it after the drop.
    let dispatcher = Arc::new(Dispatcher::new(
        Arc::clone(&repo),
        FrontendRegistry::new(),
        Arc::clone(&clock),
    ));
    let paths = ltmrs_daemon::runtime::RuntimePaths::resolve(dir.path(), "resend");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    let listener = ltmrs_daemon::runtime::bind_listener(&paths).unwrap();
    let socket = paths.endpoint.clone();

    // Connection 1: handshake normally, then commit the request and
    // drop WITHOUT responding — a deterministic unknown outcome.
    // Connection 2 serves normally (real server path, including the
    // resume handshake).
    let server_task = tokio::spawn({
        let dispatcher = Arc::clone(&dispatcher);
        async move {
            use ltmrs_daemon::envelope::WireMessage;
            let mut conn1 = listener.accept().await.unwrap();
            let m1 = read_msg(&mut conn1).await;
            let WireMessage::Handshake(hs_req) = m1 else {
                panic!("expected handshake first");
            };
            assert_eq!(hs_req.resume_retry_epoch, None);
            let hs = dispatcher.handle_handshake(&hs_req).unwrap();
            // The epoch the frontend must resume on reconnect.
            let first_epoch = hs.retry_epoch;
            write_msg(
                &mut conn1,
                &ltmrs_daemon::envelope::WireReply::Handshake(hs),
            )
            .await;
            let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                panic!("expected snapshot prefetch second");
            };
            // Serve the snapshot prefetch normally (it is part of the
            // frontend handshake path, not the mutation under test).
            let prefetch_resp = dispatcher.handle(&env).unwrap();
            write_msg(
                &mut conn1,
                &ltmrs_daemon::envelope::WireReply::Response(prefetch_resp),
            )
            .await;
            // The actual mutation: commit, then drop WITHOUT responding
            // — a deterministic unknown outcome.
            let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                panic!("expected mutation request third");
            };
            // The operation ID the frontend must resend verbatim.
            let first_op = env.operation_id;
            dispatcher.handle(&env).unwrap();
            drop(conn1);
            let mut conn2 = listener.accept().await.unwrap();
            let m2 = read_msg(&mut conn2).await;
            let WireMessage::Handshake(resume_req) = m2 else {
                panic!("expected resume handshake on conn2");
            };
            assert_eq!(
                resume_req.resume_retry_epoch,
                Some(first_epoch),
                "reconnect must resume the same epoch"
            );
            let hs2 = dispatcher.handle_handshake(&resume_req).unwrap();
            assert_eq!(hs2.retry_epoch, first_epoch);
            write_msg(
                &mut conn2,
                &ltmrs_daemon::envelope::WireReply::Handshake(hs2),
            )
            .await;
            let WireMessage::Request(env2) = read_msg(&mut conn2).await else {
                panic!("expected resent request on conn2");
            };
            assert_eq!(
                env2.operation_id, first_op,
                "reconnect must resend the same operation"
            );
            let resp = dispatcher.handle(&env2).unwrap();
            write_msg(
                &mut conn2,
                &ltmrs_daemon::envelope::WireReply::Response(resp),
            )
            .await;
        }
    });

    let client = IpcClient::new(socket);
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        ),
        client,
    );
    let mut memory = mem(7, None, 0.5, "Resend Me");
    memory.external_alias = None;
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fe.roundtrip_with_rehandshake(DomainRequest::AddMemory { memory }, false),
    )
    .await
    .expect("resend must not hang")
    .expect("unknown-outcome call must resolve");
    assert!(
        matches!(resp.result, IpcResult::Success { .. }),
        "resend must replay the recorded outcome, got {:?}",
        resp.result
    );
    server_task.await.unwrap();
    // Exactly one memory: the resend replayed, never re-executed (a
    // second AddMemory with the same ID fails as "already exists").
    let memories = repo
        .export_snapshot()
        .unwrap()
        .memories
        .into_iter()
        .filter(|m| m.title == "Resend Me")
        .collect::<Vec<_>>();
    assert_eq!(memories.len(), 1, "exactly one effect allowed");

    drop(fe);
}

/// P1 unknown-outcome + generation cut: after a transport loss, a
/// restore bumps the generation before the resume handshake. The
/// resume is refused as GenerationMismatch — but the dropped request
/// may have committed before its response was lost, so a fresh
/// mutation with the same body would double-apply increment-like
/// operations (Feedback +0.02, with no duplicate-ID guard to hide
/// behind). The frontend must surface an unknown-outcome error and
/// send nothing further. Deterministic: every drop point is
/// server-controlled, not timed (the only timeout proves the absence
/// of a reconnect).
#[tokio::test]
async fn unknown_outcome_generation_cut_surfaces_unknown_without_fresh_mutation() {
    use ltmrs_daemon::dispatcher::Dispatcher;
    use ltmrs_daemon::registry::FrontendRegistry;
    use ltmrs_domain::clock::{Clock, FrozenClock};
    use ltmrs_service::repository::CanonicalRepository;
    use tokio::io::AsyncReadExt;

    async fn read_msg(
        stream: &mut ltmrs_daemon::runtime::IpcStream,
    ) -> ltmrs_daemon::envelope::WireMessage {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await.unwrap();
        serde_json::from_slice(&buf).unwrap()
    }
    async fn write_msg(
        stream: &mut ltmrs_daemon::runtime::IpcStream,
        reply: &ltmrs_daemon::envelope::WireReply,
    ) {
        let payload = serde_json::to_vec(reply).unwrap();
        ltmrs_daemon::envelope::write_response_payload(stream, &payload)
            .await
            .unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(
            dir.path().join("store").to_str().unwrap(),
            Arc::clone(&clock),
        )
        .unwrap(),
    );
    // Seed one memory at confidence 0.5: each positive Feedback adds
    // +0.015 and bumps positive_feedback by 1, so exactly-once reads
    // (0.515, 1) and a double-apply reads (0.53, 2) — no
    // duplicate-ID guard to hide behind.
    let mut seed = mem(7, None, 0.5, "Cut Feedback");
    seed.external_alias = None;
    repo.put_memory_direct(&seed).unwrap();
    let target = seed.id;
    let dispatcher = Arc::new(Dispatcher::new(
        Arc::clone(&repo),
        FrontendRegistry::new(),
        Arc::clone(&clock),
    ));
    let paths = ltmrs_daemon::runtime::RuntimePaths::resolve(dir.path(), "cut");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    let listener = ltmrs_daemon::runtime::bind_listener(&paths).unwrap();
    let socket = paths.endpoint.clone();

    let server_task = tokio::spawn({
        let dispatcher = Arc::clone(&dispatcher);
        let repo = Arc::clone(&repo);
        async move {
            use ltmrs_daemon::envelope::WireMessage;
            let mut conn1 = listener.accept().await.unwrap();
            let WireMessage::Handshake(hs_req) = read_msg(&mut conn1).await else {
                panic!("expected handshake first");
            };
            assert_eq!(hs_req.resume_retry_epoch, None);
            let hs = dispatcher.handle_handshake(&hs_req).unwrap();
            let first_epoch = hs.retry_epoch;
            write_msg(
                &mut conn1,
                &ltmrs_daemon::envelope::WireReply::Handshake(hs),
            )
            .await;
            let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                panic!("expected snapshot prefetch second");
            };
            let prefetch_resp = dispatcher.handle(&env).unwrap();
            write_msg(
                &mut conn1,
                &ltmrs_daemon::envelope::WireReply::Response(prefetch_resp),
            )
            .await;
            // The mutation under test: commit, then drop WITHOUT
            // responding — a deterministic unknown outcome.
            let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                panic!("expected mutation request third");
            };
            dispatcher.handle(&env).unwrap();
            drop(conn1);
            // Generation cut AFTER the commit, BEFORE the reconnect:
            // the backup/restore the reviewer describes.
            let live = repo.store_generation().unwrap().as_u64();
            repo.set_store_generation(ltmrs_domain::id::StoreGeneration::new(live + 1))
                .unwrap();
            // Reconnect with a resume for the pre-cut epoch, served
            // through the real handshake path (must refuse: the
            // generation moved under the uncertain mutation).
            let mut conn2 = listener.accept().await.unwrap();
            let WireMessage::Handshake(resume_req) = read_msg(&mut conn2).await else {
                panic!("expected resume handshake on conn2");
            };
            assert_eq!(
                resume_req.resume_retry_epoch,
                Some(first_epoch),
                "reconnect must resume the same epoch"
            );
            let handshake_err = dispatcher.handle_handshake(&resume_req).unwrap_err();
            let ltmrs_daemon::envelope::IpcError::GenerationMismatch { .. } = handshake_err else {
                panic!("resume across a generation cut must mismatch");
            };
            // Production wire encoding (server.rs): kind tag + the
            // typed error's Display, which the client parses back.
            write_msg(
                &mut conn2,
                &ltmrs_daemon::envelope::WireReply::Error(ltmrs_daemon::envelope::WireError {
                    kind: "generation_mismatch".to_string(),
                    message: handshake_err.to_string(),
                }),
            )
            .await;
            // Production keeps the stream open after a mismatch
            // (same-stream retry): serve whatever follows. Pre-fix the
            // client sends a fresh handshake + prefetch + fresh
            // mutation here — the defect (a second Feedback
            // application). Post-fix it closes its end and sends
            // nothing: EOF (or a quiet timeout) is the fixed-world
            // signal.
            let mut len_buf = [0u8; 4];
            let followed = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::io::AsyncReadExt::read_exact(&mut conn2, &mut len_buf),
            )
            .await
            .is_ok_and(|r| r.is_ok());
            if !followed {
                // Fixed world: nothing further sent.
            } else {
                let len = u32::from_be_bytes(len_buf) as usize;
                let mut buf = vec![0u8; len];
                tokio::io::AsyncReadExt::read_exact(&mut conn2, &mut buf)
                    .await
                    .unwrap();
                let next: ltmrs_daemon::envelope::WireMessage =
                    serde_json::from_slice(&buf).unwrap();
                match next {
                    WireMessage::Handshake(fresh_req) => {
                        assert_eq!(fresh_req.resume_retry_epoch, None);
                        // The fresh handshake still carries the pre-cut
                        // generation: production refuses it again (kept
                        // open), the client adopts the live generation and
                        // redials. Serve that full dance faithfully.
                        let second = dispatcher.handle_handshake(&fresh_req).unwrap_err();
                        assert!(
                            matches!(
                                second,
                                ltmrs_daemon::envelope::IpcError::GenerationMismatch { .. }
                            ),
                            "fresh handshake at the old generation must mismatch, got {second:?}"
                        );
                        write_msg(
                            &mut conn2,
                            &ltmrs_daemon::envelope::WireReply::Error(
                                ltmrs_daemon::envelope::WireError {
                                    kind: "generation_mismatch".to_string(),
                                    message: second.to_string(),
                                },
                            ),
                        )
                        .await;
                        let mut conn3 = tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            listener.accept(),
                        )
                        .await
                        .expect("client must redial after adopting the live generation")
                        .unwrap();
                        let WireMessage::Handshake(redial_req) = read_msg(&mut conn3).await else {
                            panic!("expected redial handshake on conn3");
                        };
                        let hs = dispatcher.handle_handshake(&redial_req).unwrap();
                        write_msg(
                            &mut conn3,
                            &ltmrs_daemon::envelope::WireReply::Handshake(hs),
                        )
                        .await;
                        let WireMessage::Request(env) = read_msg(&mut conn3).await else {
                            panic!("expected prefetch after redial handshake");
                        };
                        let prefetch_resp = dispatcher.handle(&env).unwrap();
                        write_msg(
                            &mut conn3,
                            &ltmrs_daemon::envelope::WireReply::Response(prefetch_resp),
                        )
                        .await;
                        let WireMessage::Request(env) = read_msg(&mut conn3).await else {
                            panic!("expected fresh mutation after prefetch");
                        };
                        let fresh_resp = dispatcher.handle(&env).unwrap();
                        write_msg(
                            &mut conn3,
                            &ltmrs_daemon::envelope::WireReply::Response(fresh_resp),
                        )
                        .await;
                    }
                    _ => panic!("only a fresh handshake may follow a mismatch"),
                }
            }
        }
    });

    let client = IpcClient::new(socket);
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        ),
        client,
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fe.roundtrip_with_rehandshake(
            DomainRequest::Feedback {
                memory_id: target,
                useful: true,
            },
            false,
        ),
    )
    .await
    .expect("unknown-outcome call must not hang");
    // Server-side absence proof first: pre-fix this panics with the
    // defect named (a fresh mutation was sent); post-fix it passes.
    server_task.await.unwrap();
    let err = result.expect_err(
        "generation cut after transport loss must surface unknown outcome, never resolve",
    );
    assert!(
        err.message.contains("unknown outcome"),
        "error must name the unknown outcome, got: {}",
        err.message
    );
    // Exactly one Feedback application: +0.015 and counter 1 —
    // never +0.03 / counter 2.
    let memory = repo.get_memories(&[target]).unwrap().remove(0);
    assert_eq!(
        memory.positive_feedback, 1,
        "exactly one Feedback application allowed"
    );
    assert!(
        (memory.confidence - 0.515).abs() < 1e-9,
        "exactly one Feedback application allowed, got {}",
        memory.confidence
    );

    drop(fe);
}

/// P1 (tool replay over IPC): a ToolCall memory_add whose response is
/// lost in transport must resolve on resend through the recorded
/// receipt — not fail in the compatibility dedup scan against the
/// memory the first delivery created. Same shape as the native
/// unknown-outcome test, but driving the full ToolCall path.
#[tokio::test]
async fn toolcall_add_resend_after_resume_replays_success() {
    use ltmrs_daemon::dispatcher::Dispatcher;
    use ltmrs_daemon::registry::FrontendRegistry;
    use ltmrs_domain::clock::{Clock, FrozenClock};
    use ltmrs_service::repository::CanonicalRepository;
    use tokio::io::AsyncReadExt;

    async fn read_msg(
        stream: &mut ltmrs_daemon::runtime::IpcStream,
    ) -> ltmrs_daemon::envelope::WireMessage {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await.unwrap();
        serde_json::from_slice(&buf).unwrap()
    }
    async fn write_msg(
        stream: &mut ltmrs_daemon::runtime::IpcStream,
        reply: &ltmrs_daemon::envelope::WireReply,
    ) {
        let payload = serde_json::to_vec(reply).unwrap();
        ltmrs_daemon::envelope::write_response_payload(stream, &payload)
            .await
            .unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(
            dir.path().join("store").to_str().unwrap(),
            Arc::clone(&clock),
        )
        .unwrap(),
    );
    let dispatcher = Arc::new(Dispatcher::new(
        Arc::clone(&repo),
        FrontendRegistry::new(),
        Arc::clone(&clock),
    ));
    let paths = ltmrs_daemon::runtime::RuntimePaths::resolve(dir.path(), "tool-resend");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    let listener = ltmrs_daemon::runtime::bind_listener(&paths).unwrap();
    let socket = paths.endpoint.clone();

    let server_task = tokio::spawn({
        let dispatcher = Arc::clone(&dispatcher);
        async move {
            use ltmrs_daemon::envelope::WireMessage;
            // conn1: handshake, prefetch, ToolCall add → commit, then
            // drop WITHOUT responding (deterministic unknown outcome).
            let mut conn1 = listener.accept().await.unwrap();
            let WireMessage::Handshake(hs_req) = read_msg(&mut conn1).await else {
                panic!("expected handshake first");
            };
            assert_eq!(hs_req.resume_retry_epoch, None);
            let hs = dispatcher.handle_handshake(&hs_req).unwrap();
            let first_epoch = hs.retry_epoch;
            write_msg(
                &mut conn1,
                &ltmrs_daemon::envelope::WireReply::Handshake(hs),
            )
            .await;
            let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                panic!("expected snapshot prefetch second");
            };
            let prefetch_resp = dispatcher.handle(&env).unwrap();
            write_msg(
                &mut conn1,
                &ltmrs_daemon::envelope::WireReply::Response(prefetch_resp),
            )
            .await;
            let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                panic!("expected ToolCall request third");
            };
            let first_op = env.operation_id;
            dispatcher.handle(&env).unwrap();
            drop(conn1);
            // conn2: resume the same epoch; the resent envelope must
            // replay through the recorded receipt.
            let mut conn2 = listener.accept().await.unwrap();
            let WireMessage::Handshake(resume_req) = read_msg(&mut conn2).await else {
                panic!("expected resume handshake on conn2");
            };
            assert_eq!(
                resume_req.resume_retry_epoch,
                Some(first_epoch),
                "reconnect must resume the same epoch"
            );
            let hs2 = dispatcher.handle_handshake(&resume_req).unwrap();
            assert_eq!(hs2.retry_epoch, first_epoch);
            write_msg(
                &mut conn2,
                &ltmrs_daemon::envelope::WireReply::Handshake(hs2),
            )
            .await;
            let WireMessage::Request(env2) = read_msg(&mut conn2).await else {
                panic!("expected resent request on conn2");
            };
            assert_eq!(
                env2.operation_id, first_op,
                "reconnect must resend the same operation"
            );
            let resp = dispatcher.handle(&env2).unwrap();
            write_msg(
                &mut conn2,
                &ltmrs_daemon::envelope::WireReply::Response(resp),
            )
            .await;
        }
    });

    let client = IpcClient::new(socket);
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        ),
        client,
    );
    let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "IPC resend replay fixture fragment".to_string(),
        ..Default::default()
    });
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fe.roundtrip_with_rehandshake(DomainRequest::ToolCall { tool }, false),
    )
    .await
    .expect("resend must not hang")
    .expect("unknown-outcome ToolCall must resolve");
    let text = match &resp.result {
        ltmrs_daemon::envelope::IpcResult::Success { payload, .. } => match payload {
            ltmrs_daemon::envelope::DomainPayload::ToolResult { text, .. } => text.clone(),
            other => panic!("expected tool result, got {other:?}"),
        },
        other => panic!("resend must succeed, got {other:?}"),
    };
    assert!(
        text.contains("Added fragment"),
        "resend must replay the add, got: {text}"
    );
    server_task.await.unwrap();
    let memories = repo
        .export_snapshot()
        .unwrap()
        .memories
        .into_iter()
        .filter(|m| m.fragment == "IPC resend replay fixture fragment")
        .collect::<Vec<_>>();
    assert_eq!(memories.len(), 1, "exactly one effect allowed");

    drop(fe);
}

/// Wave B1: non-empty store must surface in the FIRST initialize.
/// Unix-socket test double (`UnixStream::pair`); Windows IPC is named pipes.
#[cfg(unix)]
#[tokio::test]
async fn first_initialize_sees_nonempty_store() {
    use ltmrs_daemon::runtime::RuntimePaths;
    use ltmrs_daemon::server::connection::handle_connection;
    use ltmrs_daemon::server::{Daemon, DaemonConfig};
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::resolve(dir.path(), "test-store");
    let config = DaemonConfig {
        store_path: dir.path().join("store").to_str().unwrap().to_string(),
        ..Default::default()
    };
    let daemon = Daemon::start(&paths, config).await.unwrap();
    daemon
        .dispatcher_arc()
        .repo()
        .put_memory_direct(&mem(9001, None, 0.9, "Prefetch Fixture"))
        .unwrap();
    let dispatcher = daemon.dispatcher_arc();
    let quotas = daemon.quotas();
    let (server_end, client_end) = tokio::net::UnixStream::pair().unwrap();
    tokio::spawn(async move {
        let _ = handle_connection(Box::new(server_end), dispatcher, quotas).await;
    });
    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(Box::new(client_end));
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        ),
        client,
    );
    let instructions = fe.get_info().instructions.unwrap();
    assert!(
        instructions.contains("no saved memories yet"),
        "unprefetched first initialize still shows the empty window, got: {instructions}"
    );
    fe.prefetch_snapshot().await;
    let instructions = fe.get_info().instructions.unwrap();
    assert!(
        !instructions.contains("no saved memories yet"),
        "prefetched first initialize must list the non-empty store, got empty-state window"
    );
    assert!(
        instructions.contains("Prefetch Fixture"),
        "first initialize must name the stored fragment, got: {instructions}"
    );
    drop(daemon);
}

/// Wave B1: prefetch over an empty store keeps the current empty-state
/// text (no behavior change for fresh stores).
/// Unix-socket test double (`UnixStream::pair`); Windows IPC is named pipes.
#[cfg(unix)]
#[tokio::test]
async fn prefetch_on_empty_store_keeps_empty_state() {
    use ltmrs_daemon::runtime::RuntimePaths;
    use ltmrs_daemon::server::connection::handle_connection;
    use ltmrs_daemon::server::{Daemon, DaemonConfig};
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::resolve(dir.path(), "test-store");
    let config = DaemonConfig {
        store_path: dir.path().join("store").to_str().unwrap().to_string(),
        ..Default::default()
    };
    let daemon = Daemon::start(&paths, config).await.unwrap();
    let dispatcher = daemon.dispatcher_arc();
    let quotas = daemon.quotas();
    let (server_end, client_end) = tokio::net::UnixStream::pair().unwrap();
    tokio::spawn(async move {
        let _ = handle_connection(Box::new(server_end), dispatcher, quotas).await;
    });
    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(Box::new(client_end));
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(3)),
            ChannelId::new(Uuid::from_u128(4)),
        ),
        client,
    );
    fe.prefetch_snapshot().await;
    let instructions = fe.get_info().instructions.unwrap();
    assert!(
        instructions.contains("no saved memories yet"),
        "empty store must keep the empty-state text, got: {instructions}"
    );
    drop(daemon);
}

/// Wave B1 managed path: the default `socket=None` path arrives
/// already handshaken (probe handshake sets the retry epoch before the
/// frontend is built). Prefetch must fetch regardless of handshake
/// state, so `get_info` is populated here too.
/// Unix-socket test double (`UnixStream::pair`); Windows IPC is named pipes.
#[cfg(unix)]
#[tokio::test]
async fn prefetch_fetches_when_already_handshaken() {
    use ltmrs_daemon::envelope::{HandshakeRequest, PROTOCOL_VERSION};
    use ltmrs_daemon::runtime::RuntimePaths;
    use ltmrs_daemon::server::connection::handle_connection;
    use ltmrs_daemon::server::{Daemon, DaemonConfig};
    use ltmrs_domain::id::StoreGeneration;
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::resolve(dir.path(), "test-store");
    let config = DaemonConfig {
        store_path: dir.path().join("store").to_str().unwrap().to_string(),
        ..Default::default()
    };
    let daemon = Daemon::start(&paths, config).await.unwrap();
    daemon
        .dispatcher_arc()
        .repo()
        .put_memory_direct(&mem(9002, None, 0.9, "Managed Prefetch Fixture"))
        .unwrap();
    let dispatcher = daemon.dispatcher_arc();
    let quotas = daemon.quotas();
    let (server_end, client_end) = tokio::net::UnixStream::pair().unwrap();
    tokio::spawn(async move {
        let _ = handle_connection(Box::new(server_end), dispatcher, quotas).await;
    });
    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(Box::new(client_end));
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(5)),
            ChannelId::new(Uuid::from_u128(6)),
        ),
        client,
    );
    {
        let mut guard = fe.client.lock().await;
        let req = HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(5)),
            channel_id: ChannelId::new(Uuid::from_u128(6)),
            resume_retry_epoch: None,
        };
        guard.handshake(&req).await.unwrap();
        assert!(guard.retry_epoch().is_some());
    }
    assert!(
        fe.get_info()
            .instructions
            .unwrap()
            .contains("no saved memories yet"),
        "setup: snapshot must start empty while handshaken"
    );
    fe.prefetch_snapshot().await;
    let instructions = fe.get_info().instructions.unwrap();
    assert!(
        !instructions.contains("no saved memories yet"),
        "prefetch on a handshaken client must fetch, got empty-state window"
    );
    assert!(
        instructions.contains("Managed Prefetch Fixture"),
        "prefetch on a handshaken client must name the stored fragment, got: {instructions}"
    );
    drop(daemon);
}

// ---- Wave B4: MCP resources + unknown-tool errors ----

fn unconnected_frontend() -> LtmrsFrontend {
    LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(41)),
            ChannelId::new(Uuid::from_u128(42)),
        ),
        IpcClient::new(std::path::PathBuf::from("unused")),
    )
}

/// Unix-socket test double (`UnixStream::pair` drives `handle_connection`
/// directly); Windows IPC is named pipes, so this helper is unix-only.
#[cfg(unix)]
async fn live_frontend_with(
    seeds: &[ltmrs_domain::memory::Memory],
) -> (
    LtmrsFrontend,
    tempfile::TempDir,
    ltmrs_daemon::server::Daemon,
) {
    use ltmrs_daemon::runtime::RuntimePaths;
    use ltmrs_daemon::server::connection::handle_connection;
    use ltmrs_daemon::server::{Daemon, DaemonConfig};
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::resolve(dir.path(), "test-store");
    let config = DaemonConfig {
        store_path: dir.path().join("store").to_str().unwrap().to_string(),
        ..Default::default()
    };
    let daemon = Daemon::start(&paths, config).await.unwrap();
    for m in seeds {
        daemon.dispatcher_arc().repo().put_memory_direct(m).unwrap();
    }
    let dispatcher = daemon.dispatcher_arc();
    let quotas = daemon.quotas();
    let (server_end, client_end) = tokio::net::UnixStream::pair().unwrap();
    tokio::spawn(async move {
        let _ = handle_connection(Box::new(server_end), dispatcher, quotas).await;
    });
    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(Box::new(client_end));
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(43)),
            ChannelId::new(Uuid::from_u128(44)),
        ),
        client,
    );
    (fe, dir, daemon)
}

fn unattributed_identity() -> ltmrs_daemon::envelope::EnvelopeIdentity {
    ltmrs_daemon::envelope::EnvelopeIdentity::default()
}

/// Unix-only helper: only the gated `live_frontend_with` callers use it.
#[cfg(unix)]
fn stable_identity_named(name: &str, root: &str) -> ltmrs_daemon::envelope::EnvelopeIdentity {
    let (full, _) = ltmrs_domain::project_id::generate_project_id(name);
    ltmrs_daemon::envelope::EnvelopeIdentity {
        project_id: Some(full),
        canonical_root: Some(root.to_string()),
        legacy_project: None,
        legacy_explicit: false,
    }
}

/// Unix-only helper: only the gated `live_frontend_with` callers use it.
#[cfg(unix)]
fn sample_guide(name: &str) -> ltmrs_domain::guide::Guide {
    ltmrs_domain::guide::Guide {
        name: name.to_string(),
        category: "dev-tool".to_string(),
        description: "test guide".to_string(),
        contexts: vec![],
        learnings: vec![],
        usage_count: 0,
        last_used: None,
        success_count: 0,
        failure_count: 0,
        anti_patterns: vec![],
        pitfalls: vec![],
        depends_on: vec![],
        enables: vec![],
        source_memories: vec![],
        validated_by: vec![],
        superseded_by: None,
        deprecated: false,
        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
        created_at: ltmrs_domain::memory::Instant::new(0),
        updated_at: ltmrs_domain::memory::Instant::new(0),
    }
}

fn resource_text(out: &rmcp::model::ReadResourceResult) -> Result<String, String> {
    use rmcp::model::ResourceContents;
    out.contents
        .iter()
        .filter_map(|c| match c {
            ResourceContents::TextResourceContents { text, .. } => Some(text.clone()),
            _ => None,
        })
        .next()
        .ok_or_else(|| format!("no text content in resource result: {out:?}"))
}

/// Unix-only helper: only the gated `live_frontend_with` callers use it.
#[cfg(unix)]
async fn add_via_tool(
    fe: &LtmrsFrontend,
    fragment: &str,
    project: Option<&str>,
    identity: &ltmrs_daemon::envelope::EnvelopeIdentity,
) -> String {
    use ltmrs_daemon::envelope::{DomainPayload, IpcResult};
    let body = DomainRequest::ToolCall {
        tool: ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: fragment.to_string(),
            project: project.map(|p| p.to_string()),
            ..Default::default()
        }),
    };
    let resp = fe.send_with_identity(body, identity).await.unwrap();
    match resp.result {
        IpcResult::Success {
            payload:
                DomainPayload::ToolResult {
                    structured: Some(s),
                    ..
                },
            ..
        } => s
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("add response has no id: {s}"))
            .to_string(),
        other => panic!("add failed: {other:?}"),
    }
}

/// `resources/list` advertises exactly the system-prompt resource
/// (upstream serves one static entry; per-item URIs are NOT enumerated).
#[test]
fn resources_list_advertises_system_prompt_only() {
    let listed = LtmrsFrontend::lemma_resource_list();
    assert_eq!(listed.len(), 1, "exactly one static entry, got: {listed:?}");
    assert_eq!(listed[0].uri, "lemma://system-prompt");
    assert_eq!(listed[0].mime_type.as_deref(), Some("text/markdown"));
}

/// `resources/read lemma://system-prompt` serves the frozen base prompt
/// text (static, deterministic — never the dynamic index).
#[tokio::test]
async fn system_prompt_resource_returns_template() {
    let fe = unconnected_frontend();
    let out = fe
        .read_lemma_resource("lemma://system-prompt", &unattributed_identity())
        .await
        .unwrap();
    let text = resource_text(&out).unwrap();
    assert!(
        text.contains("Persistent Memory"),
        "system prompt must be the frozen base template, got: {text}"
    );
    assert!(
        text.contains("RECALL"),
        "system prompt must carry the recall contract, got: {text}"
    );
    assert!(
        !text.contains("no saved memories yet"),
        "resource must not append the dynamic index, got: {text}"
    );
}

/// `resources/read lemma://memory/{id}` returns the record as JSON.
/// Unix-only: drives the unix-socket `live_frontend_with` helper.
#[cfg(unix)]
#[tokio::test]
async fn memory_resource_returns_record_json() {
    let (fe, _dir, _daemon) = live_frontend_with(&[]).await;
    let id = add_via_tool(
        &fe,
        "## Resource Fixture\n\n### Context\nResource record body.",
        None,
        &unattributed_identity(),
    )
    .await;
    let out = fe
        .read_lemma_resource(&format!("lemma://memory/{id}"), &unattributed_identity())
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&resource_text(&out).unwrap()).unwrap();
    assert_eq!(value["id"], serde_json::json!(id));
    assert!(
        value["fragment"]
            .as_str()
            .unwrap()
            .contains("Resource record body")
    );
}

/// Unknown memory ids are resource errors, not tool errors.
/// Unix-only: drives the unix-socket `live_frontend_with` helper.
#[cfg(unix)]
#[tokio::test]
async fn unknown_memory_resource_errors() {
    let (fe, _dir, _daemon) = live_frontend_with(&[]).await;
    let err = fe
        .read_lemma_resource("lemma://memory/m does-not-exist", &unattributed_identity())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "unknown memory must be a not-found resource error, got: {err}"
    );
}

/// `resources/read lemma://guides/{name}` is case-insensitive;
/// unknown guides are resource errors.
/// Unix-only: drives the unix-socket `live_frontend_with` helper.
#[cfg(unix)]
#[tokio::test]
async fn guide_resource_case_insensitive() {
    let (fe, _dir, daemon) = live_frontend_with(&[]).await;
    daemon
        .dispatcher_arc()
        .repo()
        .put_guide(&sample_guide("Rust"))
        .unwrap();
    let out = fe
        .read_lemma_resource("lemma://guides/RUST", &unattributed_identity())
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&resource_text(&out).unwrap()).unwrap();
    assert_eq!(value["guide"], serde_json::json!("Rust"));
    let err = fe
        .read_lemma_resource("lemma://guides/nope", &unattributed_identity())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "unknown guide must be a not-found resource error, got: {err}"
    );
}

/// Unknown resource URIs are resource errors.
#[tokio::test]
async fn unknown_resource_uri_errors() {
    let fe = unconnected_frontend();
    let err = fe
        .read_lemma_resource("lemma://nope/x", &unattributed_identity())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("Unknown resource"),
        "unknown URI must be an unknown-resource error, got: {err}"
    );
}

/// Unknown tools return an isError tool result, not a JSON-RPC error.
#[test]
fn unknown_tool_returns_is_error_result() {
    assert!(!LtmrsFrontend::is_known_tool("nope"));
    assert!(LtmrsFrontend::is_known_tool("memory_read"));
    let result = LtmrsFrontend::unknown_tool_result("nope");
    assert!(
        result.is_error.unwrap_or(false),
        "unknown tool must set isError"
    );
    let text = result
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("nope"),
        "unknown-tool result must name the tool, got: {text}"
    );
}

/// A stable identity cannot read a foreign project's memory resource,
/// but the same record is served without identity.
/// Unix-only: drives the unix-socket `live_frontend_with` helper.
#[cfg(unix)]
#[tokio::test]
async fn memory_resource_stable_scope_refused() {
    let (fe, _dir, _daemon) = live_frontend_with(&[]).await;
    let other = ltmrs_daemon::envelope::EnvelopeIdentity {
        project_id: None,
        canonical_root: None,
        legacy_project: Some("other".to_string()),
        legacy_explicit: true,
    };
    let id = add_via_tool(
        &fe,
        "## Foreign Fixture\n\n### Context\nForeign body.",
        Some("other"),
        &other,
    )
    .await;
    let foreign = stable_identity_named("shop", "/tmp/a/shop");
    add_via_tool(
        &fe,
        "## Shop Own Fixture\n\n### Context\nShop body.",
        None,
        &foreign,
    )
    .await;
    let err = fe
        .read_lemma_resource(&format!("lemma://memory/{id}"), &foreign)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "foreign resource must be refused to a stable identity, got: {err}"
    );
    let out = fe
        .read_lemma_resource(&format!("lemma://memory/{id}"), &unattributed_identity())
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&resource_text(&out).unwrap()).unwrap();
    assert!(value["fragment"].as_str().unwrap().contains("Foreign body"));
}
