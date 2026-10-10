//! MCP stale-path / resume / bridge tests (moved verbatim from `connection_tests.rs`).

use std::sync::Arc;

use super::test_support::*;
use super::{FrontendIdentity, LtmrsFrontend};
use ltmrs_daemon::client::IpcClient;
use ltmrs_daemon::envelope::{DomainRequest, IpcResult};
use ltmrs_domain::id::{ChannelId, FrontendId};
use uuid::Uuid;

/// I-2 stale-path resend: after a StaleGeneration answer the frontend
/// sends a FRESH envelope; when THAT send loses its response in
/// transport, the frontend must resend the fresh envelope (same op id)
/// after resume instead of surfacing a generic error the host would
/// retry as new work (double-apply). Deterministic: every drop point
/// is server-controlled, not timed.
#[tokio::test]
async fn stale_refresh_resends_fresh_envelope_after_drop() {
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
            .unwrap()
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
    let paths = ltmrs_daemon::runtime::RuntimePaths::resolve(dir.path(), "stale-resend");
    let listener = ltmrs_daemon::runtime::bind_listener(&paths).unwrap();
    let socket = paths.endpoint.clone();

    let server_task = tokio::spawn({
        let dispatcher = Arc::clone(&dispatcher);
        let repo = Arc::clone(&repo);
        async move {
            use ltmrs_daemon::envelope::{IpcResponse, WireMessage, WireReply};
            // conn1: handshake, prefetch, main X (answered stale after
            // a real generation bump), then the fresh re-handshake.
            let mut conn1 = listener.accept().await.unwrap();
            let WireMessage::Handshake(hs_req) = read_msg(&mut conn1).await else {
                panic!("expected handshake first");
            };
            let hs = dispatcher.handle_handshake(&hs_req).unwrap();
            write_msg(&mut conn1, &WireReply::Handshake(hs)).await;
            let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                panic!("expected snapshot prefetch second");
            };
            let prefetch_resp = dispatcher.handle(&env).unwrap();
            write_msg(&mut conn1, &WireReply::Response(prefetch_resp)).await;
            let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                panic!("expected main request third");
            };
            // Real generation bump: the live store moves under the call.
            let live = repo.store_generation().unwrap().as_u64();
            repo.set_store_generation(ltmrs_domain::id::StoreGeneration::new(live + 1))
                .unwrap();
            match dispatcher.handle(&env) {
                Ok(resp) => write_msg(&mut conn1, &WireReply::Response(resp)).await,
                Err(e) => {
                    write_msg(
                        &mut conn1,
                        &WireReply::Response(IpcResponse::error(env.operation_id, &e)),
                    )
                    .await
                }
            }
            // Fresh handshake after the stale answer (same stream):
            // the generation moved, so this is refused as a mismatch
            // and the frontend reconnects before retrying.
            let WireMessage::Handshake(hs_req) = read_msg(&mut conn1).await else {
                panic!("expected fresh handshake fourth");
            };
            assert_eq!(hs_req.resume_retry_epoch, None);
            let mismatch = ltmrs_daemon::envelope::IpcError::GenerationMismatch {
                daemon: 2,
                client: 1,
            };
            write_msg(
                &mut conn1,
                &WireReply::Error(ltmrs_daemon::envelope::WireError {
                    kind: "generation_mismatch".to_string(),
                    message: mismatch.to_string(),
                }),
            )
            .await;
            // conn1b: the frontend reconnects after the mismatch (see
            // ensure_handshaked) — handshake, prefetch, then the fresh
            // mutation, which commits and is dropped without responding.
            let mut conn1b = listener.accept().await.unwrap();
            let WireMessage::Handshake(hs_req) = read_msg(&mut conn1b).await else {
                panic!("expected handshake on conn1b");
            };
            let hs = dispatcher.handle_handshake(&hs_req).unwrap();
            let fresh_epoch = hs.retry_epoch;
            write_msg(&mut conn1b, &WireReply::Handshake(hs)).await;
            let WireMessage::Request(env) = read_msg(&mut conn1b).await else {
                panic!("expected prefetch on conn1b");
            };
            let prefetch_resp = dispatcher.handle(&env).unwrap();
            write_msg(&mut conn1b, &WireReply::Response(prefetch_resp)).await;
            let WireMessage::Request(env) = read_msg(&mut conn1b).await else {
                panic!("expected fresh mutation on conn1b");
            };
            let fresh_op = env.operation_id;
            dispatcher.handle(&env).unwrap();
            drop(conn1);
            drop(conn1b);
            // conn2: resume the fresh epoch and resend the fresh op.
            let mut conn2 = listener.accept().await.unwrap();
            let m2 = read_msg(&mut conn2).await;
            let WireMessage::Handshake(resume_req) = m2 else {
                panic!("expected resume handshake on conn2");
            };
            assert_eq!(
                resume_req.resume_retry_epoch,
                Some(fresh_epoch),
                "reconnect must resume the fresh epoch"
            );
            let hs2 = dispatcher.handle_handshake(&resume_req).unwrap();
            assert_eq!(hs2.retry_epoch, fresh_epoch);
            write_msg(&mut conn2, &WireReply::Handshake(hs2)).await;
            let WireMessage::Request(env2) = read_msg(&mut conn2).await else {
                panic!("expected resent request on conn2");
            };
            assert_eq!(
                env2.operation_id, fresh_op,
                "reconnect must resend the fresh operation"
            );
            let resp = dispatcher.handle(&env2).unwrap();
            write_msg(&mut conn2, &WireReply::Response(resp)).await;
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
    let mut memory = mem(8, None, 0.5, "Stale Refresh");
    memory.external_alias = None;
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fe.roundtrip_with_rehandshake(DomainRequest::AddMemory { memory }),
    )
    .await
    .expect("resend must not hang")
    .expect("stale-refresh call must resolve");
    assert!(
        matches!(resp.result, IpcResult::Success { .. }),
        "fresh resend must replay the recorded outcome, got {:?}",
        resp.result
    );
    server_task.await.unwrap();
    // Exactly one memory: the fresh resend replayed, never re-executed.
    let memories = repo
        .export_snapshot()
        .unwrap()
        .memories
        .into_iter()
        .filter(|m| m.title == "Stale Refresh")
        .collect::<Vec<_>>();
    assert_eq!(memories.len(), 1, "exactly one effect allowed");

    drop(fe);
}

/// Explicit StaleReplay on a healthy stream renews the epoch: the dead
/// epoch refused this fresh operation before execution (certain
/// no-commit), so the frontend forgets it, re-handshakes, and executes
/// once with a fresh operation id — instead of failing every mutation
/// until restart. A second stale answer returns as-is (bounded).
#[tokio::test]
async fn stale_replay_renews_epoch_and_executes_once() {
    use ltmrs_daemon::envelope::{DomainPayload, IpcResponse, IpcResult, WireMessage, WireReply};
    use ltmrs_daemon::runtime::RuntimePaths;
    use ltmrs_domain::command::DomainErrorCode;
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
            .unwrap()
    }

    // Fully scripted server: no real dispatch, so the flow (refusal →
    // fresh handshake → fresh op → success) is pinned exactly.
    fn handshake_reply(epoch: u64) -> ltmrs_daemon::envelope::WireReply {
        WireReply::Handshake(ltmrs_daemon::envelope::HandshakeResponse {
            protocol_version: ltmrs_daemon::envelope::PROTOCOL_VERSION,
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            retry_epoch: epoch,
        })
    }
    fn memories_reply(
        operation_id: ltmrs_domain::id::OperationId,
    ) -> ltmrs_daemon::envelope::WireReply {
        WireReply::Response(IpcResponse {
            protocol_version: ltmrs_daemon::envelope::PROTOCOL_VERSION,
            operation_id,
            result: IpcResult::Success {
                outcome: ltmrs_domain::command::ReceiptOutcome::Success { affected: vec![] },
                payload: DomainPayload::Memories(vec![]),
            },
        })
    }

    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::resolve(dir.path(), "stale-epoch");
    let listener = ltmrs_daemon::runtime::bind_listener(&paths).unwrap();
    let socket = paths.endpoint.clone();

    let server_task = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        // First handshake issues epoch 5.
        let WireMessage::Handshake(hs_req) = read_msg(&mut conn).await else {
            panic!("expected handshake first");
        };
        assert_eq!(hs_req.resume_retry_epoch, None);
        write_msg(&mut conn, &handshake_reply(5)).await;
        // Prefetch after handshake (best-effort snapshot for instructions).
        let WireMessage::Request(prefetch) = read_msg(&mut conn).await else {
            panic!("expected prefetch second");
        };
        write_msg(&mut conn, &memories_reply(prefetch.operation_id)).await;
        // First mutation under the dead epoch: explicit healthy-stream
        // refusal before any execution.
        let WireMessage::Request(env1) = read_msg(&mut conn).await else {
            panic!("expected mutation third");
        };
        assert_eq!(env1.retry_epoch, 5);
        write_msg(
            &mut conn,
            &WireReply::Response(IpcResponse {
                protocol_version: ltmrs_daemon::envelope::PROTOCOL_VERSION,
                operation_id: env1.operation_id,
                result: IpcResult::Error {
                    code: DomainErrorCode::StaleReplay,
                    message: "retry namespace expired".to_string(),
                },
            }),
        )
        .await;
        // Fresh handshake on the SAME stream (no redial for a healthy
        // refusal), never a resume: the epoch is dead, not resumable.
        let WireMessage::Handshake(fresh_req) = read_msg(&mut conn).await else {
            panic!("expected fresh handshake fourth");
        };
        assert_eq!(fresh_req.resume_retry_epoch, None);
        write_msg(&mut conn, &handshake_reply(9)).await;
        // Prefetch after the fresh handshake.
        let WireMessage::Request(prefetch2) = read_msg(&mut conn).await else {
            panic!("expected prefetch fifth");
        };
        write_msg(&mut conn, &memories_reply(prefetch2.operation_id)).await;
        // Fresh mutation: new operation id, fresh epoch, succeeds.
        let WireMessage::Request(env2) = read_msg(&mut conn).await else {
            panic!("expected fresh mutation sixth");
        };
        assert_ne!(
            env2.operation_id, env1.operation_id,
            "renewal must mint a fresh operation"
        );
        assert_eq!(env2.retry_epoch, 9);
        write_msg(
            &mut conn,
            &WireReply::Response(IpcResponse {
                protocol_version: ltmrs_daemon::envelope::PROTOCOL_VERSION,
                operation_id: env2.operation_id,
                result: IpcResult::Success {
                    outcome: ltmrs_domain::command::ReceiptOutcome::Success { affected: vec![] },
                    payload: DomainPayload::Memories(vec![]),
                },
            }),
        )
        .await;
    });

    let client = IpcClient::new(socket);
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        ),
        client,
    );
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fe.roundtrip_with_rehandshake(DomainRequest::ListMemories),
    )
    .await
    .expect("renewal must not hang")
    .expect("stale-epoch call must resolve");
    assert!(
        matches!(resp.result, IpcResult::Success { .. }),
        "renewed mutation must succeed, got {:?}",
        resp.result
    );
    server_task.await.unwrap();
    drop(fe);
}

/// Bridged post-restore regression (release Critical): the stdio bridge
/// has no socket listener to redial, so an established bridged frontend
/// must survive a generation bump transparently — stale answer,
/// epoch-only forget, same-stream re-handshake, one retry. Timeout
/// guarded: the pre-fix shape parked forever on a dead socket dial.
#[tokio::test]
async fn bridged_frontend_survives_generation_bump() {
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
    let (server_end, client_end) = tokio::io::duplex(65536);
    let (server_end, client_end) = (Box::new(server_end), Box::new(client_end));
    tokio::spawn(async move {
        let _ = handle_connection(server_end, dispatcher, quotas).await;
    });

    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(client_end);
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        ),
        client,
    );
    // Establish at generation 1.
    let first = fe
        .roundtrip_with_rehandshake(DomainRequest::ListMemories)
        .await
        .unwrap();
    assert!(
        matches!(
            first.result,
            ltmrs_daemon::envelope::IpcResult::Success { .. }
        ),
        "baseline call must serve, got {:?}",
        first.result
    );
    // Restore bumps the live generation mid-session.
    daemon
        .dispatcher_arc()
        .repo()
        .set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
        .unwrap();
    let recovered = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        fe.roundtrip_with_rehandshake(DomainRequest::ListMemories),
    )
    .await
    .expect("post-restore call must complete, never park");
    let resp = recovered.unwrap();
    assert!(
        matches!(
            resp.result,
            ltmrs_daemon::envelope::IpcResult::Success { .. }
        ),
        "established bridged connection must recover, got {:?}",
        resp.result
    );
    assert_eq!(
        fe.identity.generation(),
        ltmrs_domain::id::StoreGeneration::new(2),
        "frontend must adopt the live generation"
    );

    drop(daemon);
}

/// Sticky-stream regression: a generically rejected handshake must close
/// the (daemon-closed) stream instead of keeping a dead-but-connected
/// client that fails every later call on the same stream.
#[tokio::test]
async fn rejected_handshake_closes_dead_stream() {
    use ltmrs_daemon::envelope::{WireError, WireReply};
    use tokio::io::AsyncReadExt;

    let (server_end, client_end) = tokio::io::duplex(65536);
    let (server_end, client_end) = (Box::new(server_end), Box::new(client_end));
    tokio::spawn(async move {
        let mut server_end = server_end;
        let mut len_buf = [0u8; 4];
        if server_end.read_exact(&mut len_buf).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        if server_end.read_exact(&mut buf).await.is_err() {
            return;
        }
        let reply = WireReply::Error(WireError {
            kind: "handshake_rejected".into(),
            message: "boom".into(),
        });
        let payload = serde_json::to_vec(&reply).unwrap();
        let _ = ltmrs_daemon::envelope::write_response_payload(&mut server_end, &payload).await;
        // Daemon closes rejected handshakes: drop our end.
    });

    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(client_end);
    let fe = LtmrsFrontend::new(
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        ),
        client,
    );
    assert!(fe.ensure_handshaked().await.is_err());
    assert!(
        !fe.client.lock().await.is_connected(),
        "rejected handshake must not leave a dead-but-connected stream"
    );
}

/// Sticky-stream regression: a transport failure mid-call (broken pipe,
/// daemon restart) must forget the handshake so the next call
/// reconnects instead of failing on the dead stream forever.
#[tokio::test]
async fn transport_failure_forgets_handshake() {
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
    let paths = ltmrs_daemon::runtime::RuntimePaths::resolve(dir.path(), "test-store");
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
    assert!(fe.client.lock().await.retry_epoch().is_some());
    // Simulate a dead stream: swap in a pair end whose peer is gone.
    let (dead, peer) = tokio::io::duplex(65536);
    let peer = Box::new(peer);
    drop(peer);
    fe.client.lock().await.set_stream(Box::new(dead));
    assert!(
        fe.roundtrip_with_rehandshake(DomainRequest::ListMemories)
            .await
            .is_err()
    );
    assert!(
        fe.client.lock().await.retry_epoch().is_none(),
        "transport failure must forget the handshake epoch"
    );
    assert!(
        !fe.client.lock().await.is_connected(),
        "transport failure must drop the dead stream"
    );

    drop(daemon);
    server_handle.abort();
}
