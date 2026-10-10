//! Backend tests (moved verbatim from `backend.rs`).

use super::e5::{E5_CHUNK_VERSION, ServiceQueryEmbedder, e5_chunks_to_text_chunks};
use super::embedders::{ClosureEmbedder, NoDenseEmbedder, QueryEmbedderAdapter};
use super::{SearchBackend, SearchState};
use crate::retrieval::engine::QueryEmbedder;
use ltmrs_embeddings::e5_small::Chunk;

/// Policy versions are distinct strings: the E5 recipe must never share
/// the default single-chunk version.
#[test]
fn e5_chunk_version_differs_from_default() {
    use crate::search::projector::SINGLE_CHUNK_VERSION;
    assert!(!E5_CHUNK_VERSION.is_empty());
    assert_ne!(E5_CHUNK_VERSION, SINGLE_CHUNK_VERSION);
}

/// Service-routed queries embed with the Query role through the bounded
/// worker (design §7.3), awaiting normally — never blocking the caller.
#[tokio::test]
async fn service_backed_query_embeds_with_query_role() {
    use ltmrs_embeddings::artifacts::ArtifactResult;
    use ltmrs_embeddings::e5_small::{EmbedInput, EmbeddedSequence};
    use ltmrs_embeddings::recipe::Role;
    use ltmrs_embeddings::service::EmbeddingService;
    use ltmrs_embeddings::worker::{EmbeddingWorkerConfig, SyncEmbedder};
    use std::sync::Mutex as StdMutex;

    struct RecordingEmbedder {
        roles: std::sync::Arc<StdMutex<Vec<Role>>>,
    }
    impl SyncEmbedder for RecordingEmbedder {
        fn embed_batch(&mut self, inputs: &[EmbedInput]) -> ArtifactResult<Vec<EmbeddedSequence>> {
            self.roles
                .lock()
                .unwrap()
                .extend(inputs.iter().map(|i| i.role));
            Ok(inputs
                .iter()
                .map(|_| EmbeddedSequence {
                    vector: vec![0.25; 384],
                    input_ids: vec![],
                    attention_mask: vec![],
                })
                .collect())
        }
    }

    let roles = std::sync::Arc::new(StdMutex::new(Vec::new()));
    let svc = EmbeddingService::spawn(
        RecordingEmbedder {
            roles: std::sync::Arc::clone(&roles),
        },
        EmbeddingWorkerConfig::default(),
    );
    let provider = ServiceQueryEmbedder::new(svc);
    let vec = provider.embed_query("how to cut over").await.unwrap();
    assert_eq!(vec, vec![0.25; 384]);
    assert_eq!(*roles.lock().unwrap(), vec![Role::Query]);
}

/// A closed worker surfaces as an error, never a hang or a zero vector.
#[tokio::test]
async fn service_backed_query_after_shutdown_is_error() {
    use ltmrs_embeddings::service::EmbeddingService;
    use ltmrs_embeddings::worker::EmbeddingWorkerConfig;

    struct EmptyEmbedder;
    impl ltmrs_embeddings::worker::SyncEmbedder for EmptyEmbedder {
        fn embed_batch(
            &mut self,
            inputs: &[ltmrs_embeddings::e5_small::EmbedInput],
        ) -> ltmrs_embeddings::artifacts::ArtifactResult<
            Vec<ltmrs_embeddings::e5_small::EmbeddedSequence>,
        > {
            Ok(inputs
                .iter()
                .map(|_| ltmrs_embeddings::e5_small::EmbeddedSequence {
                    vector: vec![0.0; 384],
                    input_ids: vec![],
                    attention_mask: vec![],
                })
                .collect())
        }
    }

    let svc = EmbeddingService::spawn(EmptyEmbedder, EmbeddingWorkerConfig::default());
    svc.shutdown();
    let provider = ServiceQueryEmbedder::new(svc);
    let err = provider.embed_query("q").await.unwrap_err();
    assert!(
        err.message.contains("worker")
            || err.message.contains("Closed")
            || err.message.contains("closed"),
        "closed worker must surface, got: {}",
        err.message
    );
}

/// Overload surfaces as a retryable `busy:` error through the bridge,
/// never silent fallback material at this layer.
#[tokio::test]
async fn service_backed_query_busy_is_retryable() {
    use crate::retrieval::engine::QueryEmbedder;
    use ltmrs_embeddings::service::EmbeddingService;
    use ltmrs_embeddings::worker::{EmbeddingWorkerConfig, SyncEmbedder};

    // Gated adapter: the worker holds the first request (proven via
    // the entry signal) while the test fills the single queue slot,
    // so "worker busy + queue full" is structural, never a hope-sleep.
    // The gate replaces the 500ms fixture sleep: blocking is exact.
    struct GatedEmbedder {
        entered: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }
    impl SyncEmbedder for GatedEmbedder {
        fn embed_batch(
            &mut self,
            inputs: &[ltmrs_embeddings::e5_small::EmbedInput],
        ) -> ltmrs_embeddings::artifacts::ArtifactResult<
            Vec<ltmrs_embeddings::e5_small::EmbeddedSequence>,
        > {
            if let Some(tx) = self
                .entered
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                let _ = tx.send(());
            }
            if let Some(rx) = self
                .release
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                let _ = rx.recv();
            }
            Ok(inputs
                .iter()
                .map(|_| ltmrs_embeddings::e5_small::EmbeddedSequence {
                    vector: vec![0.0; 384],
                    input_ids: vec![],
                    attention_mask: vec![],
                })
                .collect())
        }
    }

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let svc = EmbeddingService::spawn(
        GatedEmbedder {
            entered: std::sync::Mutex::new(Some(entered_tx)),
            release: std::sync::Mutex::new(Some(release_rx)),
        },
        EmbeddingWorkerConfig {
            max_batch_size: 32,
            max_queue_depth: 1,
        },
    );
    // Occupy the worker, then fill the single queue slot.
    let w1 = tokio::spawn({
        let svc = svc.clone();
        async move {
            svc.embed("first", ltmrs_embeddings::recipe::Role::Query)
                .await
        }
    });
    // Blocking channel waits ride spawn_blocking: this test runs on
    // the current-thread runtime, where a blocking recv would starve
    // the spawned submitters it waits for.
    tokio::task::spawn_blocking(move || {
        entered_rx.recv_timeout(std::time::Duration::from_secs(10))
    })
    .await
    .unwrap()
    .expect("worker must pick up the first request");
    let w2 = tokio::spawn({
        let svc = svc.clone();
        async move {
            svc.embed("second", ltmrs_embeddings::recipe::Role::Query)
                .await
        }
    });
    // Prove the second request is queued behind the held worker: the
    // worker is gated (cannot finish) and only the queue can hold it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while svc.stats().in_flight != 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "second request must queue behind the held worker"
        );
        tokio::task::yield_now().await;
    }
    // Worker busy + queue full: the bridge must report retryable busy.
    let provider = ServiceQueryEmbedder::new(svc.clone());
    let err = provider.embed_query("third").await.unwrap_err();
    assert!(
        err.message.starts_with("busy:"),
        "overload must be retryable busy, got: {}",
        err.message
    );
    let _ = release_tx.send(());
    let _ = w1.await.unwrap();
    let _ = w2.await.unwrap();
}

/// Nested retrieve_sync through the service bridge: the production dense
/// shape (outer bridge + engine await + service await) with no nested
/// blocking. Would panic on any nested block_on.
#[tokio::test]
async fn retrieve_sync_through_service_bridge_does_not_nest_block() {
    use crate::retrieval::engine::RetrievalRequest;
    use ltmrs_embeddings::service::EmbeddingService;
    use ltmrs_embeddings::worker::{EmbeddingWorkerConfig, SyncEmbedder};

    struct ConstEmbedder;
    impl SyncEmbedder for ConstEmbedder {
        fn embed_batch(
            &mut self,
            inputs: &[ltmrs_embeddings::e5_small::EmbedInput],
        ) -> ltmrs_embeddings::artifacts::ArtifactResult<
            Vec<ltmrs_embeddings::e5_small::EmbeddedSequence>,
        > {
            CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(inputs
                .iter()
                .map(|_| ltmrs_embeddings::e5_small::EmbeddedSequence {
                    vector: vec![1.0; 384],
                    input_ids: vec![],
                    attention_mask: vec![],
                })
                .collect())
        }
    }
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    let dir = tempfile::tempdir().unwrap();
    let table = crate::search::table::SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = std::sync::Arc::new(
        ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
            .unwrap(),
    );
    let svc = EmbeddingService::spawn(ConstEmbedder, EmbeddingWorkerConfig::default());
    let backend = SearchBackend::new(
        repo,
        table,
        std::sync::Arc::new(ServiceQueryEmbedder::new(svc)),
    );
    let req = RetrievalRequest {
        query: "anything".into(),
        // Dense leg on: the engine must await the service through the
        // outer bridge with no nested blocking anywhere.
        model_fingerprint: Some(ltmrs_domain::id::ModelFingerprint::new(1)),
        ..Default::default()
    };
    // Same sync-context rule as the dispatcher: bridge from blocking code.
    let out = tokio::task::spawn_blocking(move || backend.retrieve_sync(&req))
        .await
        .unwrap()
        .unwrap();
    assert!(out.results.is_empty(), "empty table recalls nothing");
    // The dense leg ran: a future early-exit skipping the embed would
    // keep this green while voiding the nesting proof.
    assert_eq!(
        CALLS.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "dense leg must invoke the worker exactly once"
    );
}

/// search_state tracks table readiness through the same bridge contract
/// as retrieve_sync (blocking context): fresh table without an index
/// reports Partial, never Complete.
#[tokio::test]
async fn search_state_partial_without_fts_index() {
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = std::sync::Arc::new(
        ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
            .unwrap(),
    );
    let lance_dir = tempfile::tempdir().unwrap();
    let table = crate::search::table::SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let backend = SearchBackend::new(repo, table, std::sync::Arc::new(NoDenseEmbedder));
    let state = tokio::task::spawn_blocking(move || backend.search_state())
        .await
        .unwrap();
    assert!(
        matches!(&state, SearchState::Partial { reason } if reason.contains("fts")),
        "unindexed table must report Partial, got: {state:?}"
    );
}

/// search_state reports Unavailable (not Partial) when the Lance table
/// itself is unreadable: a corrupt table is not "index not built".
/// The index is created first so the readiness probe must describe
/// on-storage index state (which the destruction breaks), rather
/// than answering from the empty in-memory listing.
#[tokio::test]
async fn search_state_unavailable_when_table_unreadable() {
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = std::sync::Arc::new(
        ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
            .unwrap(),
    );
    let lance_dir = tempfile::tempdir().unwrap();
    let table = crate::search::table::SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    table.ensure_fts_index().await.unwrap();
    let backend = SearchBackend::new(repo, table, std::sync::Arc::new(NoDenseEmbedder));
    // Destroy the dataset files under the open handle: readiness
    // probes must fail, not report healthy-absent.
    std::fs::remove_dir_all(lance_dir.path()).unwrap();
    let state = tokio::task::spawn_blocking(move || backend.search_state())
        .await
        .unwrap();
    assert!(
        matches!(&state, SearchState::Unavailable { reason } if reason.contains("unreadable")),
        "corrupt table must report Unavailable, got: {state:?}"
    );
}

/// search_state reports Complete once the index is built and no
/// projection work pends: a converged empty table is authoritative,
/// including for empty answers.
#[tokio::test]
async fn search_state_complete_when_converged() {
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = std::sync::Arc::new(
        ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
            .unwrap(),
    );
    let lance_dir = tempfile::tempdir().unwrap();
    let table = crate::search::table::SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    table.ensure_fts_index().await.unwrap();
    let backend = SearchBackend::new(repo, table, std::sync::Arc::new(NoDenseEmbedder));
    let state = tokio::task::spawn_blocking(move || backend.search_state())
        .await
        .unwrap();
    assert_eq!(state, SearchState::Complete);
}

/// Live E5 embedding against a provisioned cache (ignored: needs the
/// ~500MB pinned artifacts). Gate: `LTMRS_PROBE_MODELS=<models dir>`;
/// skips (never fails) without it so the suite stays offline-safe.
/// Proves the serving-time embed path — adapter load + worker + sync
/// bridge, query and passage roles — independent of table contents.
#[tokio::test]
#[ignore]
async fn live_e5_embed_against_provisioned_cache() {
    use ltmrs_embeddings::artifacts::ArtifactCache;
    use ltmrs_embeddings::service::EmbeddingService;

    let models = std::env::var("LTMRS_PROBE_MODELS").unwrap_or_default();
    if models.is_empty() || !std::path::Path::new(&models).exists() {
        eprintln!("SKIP: set LTMRS_PROBE_MODELS to a provisioned models dir");
        return;
    }
    let cache = ArtifactCache::new(&models);
    let svc = EmbeddingService::load_e5_small_from_cache(&cache).expect("provisioned cache loads");
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = std::sync::Arc::new(
        ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
            .unwrap(),
    );
    let table_dir = tempfile::tempdir().unwrap();
    let table = crate::search::table::SearchTable::open(table_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let backend = SearchBackend::new(
        repo,
        table,
        std::sync::Arc::new(ServiceQueryEmbedder::new(svc)),
    );
    // Same sync-context rule as the dispatcher: bridge from blocking code.
    let (q, p) = tokio::task::spawn_blocking(move || {
        let q = backend.embed_query_sync("fox jumping near a river")?;
        let p = backend
            .embed_passages_sync(&["the quick brown fox jumps over the lazy dog".to_string()])?;
        Ok::<_, ltmrs_domain::command::DomainError>((q, p))
    })
    .await
    .unwrap()
    .unwrap();
    for (role, v) in [("query", &q), ("passage", &p[0])] {
        assert_eq!(v.len(), 384, "{role} dim");
        assert!(v.iter().all(|x| x.is_finite()), "{role} finite");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "{role} L2-normalized, got {norm}"
        );
    }
    assert_ne!(q, p[0], "query/passage roles differ (prefixes applied)");

    // The projection seam over a shared adapter handle: same vector
    // space, E5 chunk policy stamp, single unit for short text.
    use crate::search::projector::Embedder as _;
    use ltmrs_embeddings::e5_small::E5SmallAdapter;
    let adapter =
        E5SmallAdapter::load_from_cache(&cache).expect("provisioned cache loads for projection");
    let mut shared = std::sync::Arc::new(std::sync::Mutex::new(adapter));
    let pv = shared
        .embed("the quick brown fox jumps over the lazy dog")
        .unwrap();
    assert_eq!(pv.len(), 384, "projection dim");
    assert!(pv.iter().all(|x| x.is_finite()), "projection finite");
    let units = shared.chunk_text("T", "short fragment");
    assert_eq!(units.len(), 1, "short text is one unit");
    assert_eq!(units[0].text, "T\nshort fragment");
    assert_eq!(shared.chunker_version(), E5_CHUNK_VERSION);
}

/// Batched E5 embedding matches sequential embedding within float
/// tolerance on fixed texts (padding masks make the math identical;
/// batch dim may reorder reductions). Ignored: needs pinned artifacts.
#[tokio::test]
#[ignore]
async fn e5_batch_matches_sequential_within_tolerance() {
    use crate::search::projector::Embedder;
    use ltmrs_embeddings::artifacts::ArtifactCache;
    use ltmrs_embeddings::e5_small::E5SmallAdapter;

    let models = std::env::var("LTMRS_PROBE_MODELS").unwrap_or_default();
    if models.is_empty() || !std::path::Path::new(&models).exists() {
        eprintln!("SKIP: set LTMRS_PROBE_MODELS to a provisioned models dir");
        return;
    }
    let cache = ArtifactCache::new(&models);
    let mut adapter = E5SmallAdapter::load_from_cache(&cache).expect("provisioned cache loads");
    let texts = vec![
        "the quick brown fox jumps over the lazy dog".to_string(),
        "quantum entanglement enables instantaneous correlation".to_string(),
    ];
    let batched = adapter
        .embed_texts(&texts)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let single0 = Embedder::embed(&mut adapter, &texts[0]).unwrap();
    let single1 = Embedder::embed(&mut adapter, &texts[1]).unwrap();
    for (b, s) in batched.iter().zip([single0, single1].iter()) {
        assert_eq!(b.len(), s.len());
        for (x, y) in b.iter().zip(s.iter()) {
            assert!((x - y).abs() < 1e-5, "batched diverged: {x} vs {y}");
        }
    }
    // Production tick shape: the projector boxes the shared
    // `Arc<Mutex<E5SmallAdapter>>` handle, so the batch override must
    // fire through the wrapper's `embed_texts` forward as well.
    let shared = std::sync::Arc::new(std::sync::Mutex::new(
        E5SmallAdapter::load_from_cache(&cache).expect("provisioned cache loads"),
    ));
    let mut boxed: Box<dyn Embedder> = Box::new(std::sync::Arc::clone(&shared));
    let wrapped = boxed
        .embed_texts(&texts)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(wrapped.len(), batched.len());
    for (w, b) in wrapped.iter().zip(batched.iter()) {
        assert_eq!(w.len(), b.len());
        for (x, y) in w.iter().zip(b.iter()) {
            assert!((x - y).abs() < 1e-5, "wrapper diverged: {x} vs {y}");
        }
    }
}

/// The E5 chunk mapping re-prefixes each verbatim fragment span for
/// lexical searchability and shifts its offsets into rendered coordinates.
#[test]
fn e5_chunk_mapping_reprefixes_and_shifts_to_rendered_coords() {
    let chunks = vec![
        Chunk {
            text: "alpha".into(),
            char_start: 0,
            char_end: 5,
            token_count: 3,
        },
        Chunk {
            text: "beta".into(),
            char_start: 6,
            char_end: 10,
            token_count: 2,
        },
    ];
    let units = e5_chunks_to_text_chunks("T", &chunks);
    assert_eq!(units.len(), 2);
    assert_eq!(units[0].text, "T\nalpha");
    assert_eq!((units[0].char_start, units[0].char_end), (2, 7));
    assert_eq!(units[1].text, "T\nbeta");
    assert_eq!((units[1].char_start, units[1].char_end), (8, 12));
}

/// An empty chunk set maps to no units (the projector's own fallback
/// covers a misbehaving embedder; the mapping itself adds nothing).
#[test]
fn e5_chunk_mapping_preserves_empty() {
    assert!(e5_chunks_to_text_chunks("T", &[]).is_empty());
}

/// Serving freshness: `retrieve_sync` observes commits made after the
/// backend was constructed (production: the tick publishes through its
/// own handle; serving must not pin a stale snapshot).
#[tokio::test]
async fn retrieve_sync_sees_post_construction_commits() {
    use crate::retrieval::engine::RetrievalRequest;
    use crate::search::row::SearchRow;
    use ltmrs_domain::id::ModelFingerprint;

    let repo_dir = tempfile::tempdir().unwrap();
    let repo = std::sync::Arc::new(
        ltmrs_service::repository::CanonicalRepository::open(repo_dir.path().to_str().unwrap())
            .unwrap(),
    );
    let table_dir = tempfile::tempdir().unwrap();
    let uri = table_dir.path().to_str().unwrap();
    let table = crate::search::table::SearchTable::open(uri).await.unwrap();
    let backend = std::sync::Arc::new(SearchBackend::new(
        repo,
        table,
        std::sync::Arc::new(QueryEmbedderAdapter::new(std::sync::Arc::new(
            ClosureEmbedder::new(|_| Ok(vec![1.0; 384])),
        ))),
    ));
    // A second handle commits a dense row after construction.
    let writer = crate::search::table::SearchTable::open(uri).await.unwrap();
    writer
        .publish_rows(&[SearchRow {
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            memory_id: ltmrs_domain::id::EntityId::new(uuid::Uuid::from_u128(1)),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            model_fingerprint: ModelFingerprint::new(1),
            chunk_id: ltmrs_domain::id::ChunkId::new(0),
            chunker_version: "single-chunk-v1".to_string(),
            lexical_text: "fresh row".to_string(),
            char_start: 0,
            char_end: 9,
            project: None,
            fragment_type: "fact".to_string(),
            created_at_millis: 1,
            confidence: 0.5,
            updated_at_millis: 1,
            embedding: Some(vec![1.0; 384]),
        }])
        .await
        .unwrap();
    let req = RetrievalRequest {
        query: "fresh".to_string(),
        model_fingerprint: Some(ModelFingerprint::new(1)),
        ..Default::default()
    };
    // Same sync-context rule as the dispatcher: bridge from blocking code.
    let out = tokio::task::spawn_blocking(move || backend.retrieve_sync(&req))
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.explanation.dense_ready,
        "serving must observe the committed dense row"
    );
}
