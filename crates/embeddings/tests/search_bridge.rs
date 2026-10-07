//! Search-bridge integration tests (moved from `e5_small` unit tests in Task 6).
//!
//! These tests exercise the search-side seams (`projector::Embedder`,
//! `backend::QueryEmbedderProvider`) whose implementations live outside this
//! crate. They must run as integration tests: a unit test would link the
//! `cfg(test)` build of `ltmrs-embeddings` while `ltmrs-search` links the
//! normal build (twin crates — the search-side impls could never apply).
//! The integration target links a single normal build, so the seams resolve.

use std::path::PathBuf;
use std::sync::Mutex;

use ltmrs_embeddings::artifacts::ArtifactCache;
use ltmrs_embeddings::e5_small::E5SmallAdapter;
use ltmrs_embeddings::manifest::e5_small_artifact;
use ltmrs_embeddings::recipe::Role;
use ltmrs_search::search::backend::QueryEmbedderProvider;
use ltmrs_search::search::projector::Embedder as ProjectorEmbedder;

/// Load the reference fixture JSON from the tracked fixtures directory.
fn load_fixture() -> serde_json::Value {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/fixtures/reference_fixture.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Populate the cache directory with the pinned artifact files so tests can
/// run offline against real weights. Returns false (skip) when artifacts are
/// not present in tmp/e5-artifacts (CI without network). The artifacts live at
/// the workspace root, two levels above this crate's manifest dir.
fn populate_cache(cache: &ArtifactCache, _guard: &tempfile::TempDir) -> bool {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tmp/e5-artifacts");
    if !src.join("model.safetensors").exists() {
        return false;
    }
    let artifact = e5_small_artifact();
    let mdir = cache.model_dir(&artifact);
    std::fs::create_dir_all(&mdir).unwrap();
    for file in artifact.digests.keys() {
        let from = src.join(file);
        if from.exists() {
            std::fs::copy(&from, mdir.join(file)).unwrap();
        }
    }
    true
}

fn test_cache() -> (ArtifactCache, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (ArtifactCache::new(dir.path()), dir)
}

/// Projector bridge: the adapter exposes chunk_passage through the
/// projector Embedder seam with rendered-coordinate spans, and its
/// document embedding matches the Passage-role recipe exactly.
#[test]
fn projector_bridge_chunks_and_embeds_as_passage() {
    let (cache, guard) = test_cache();
    if !populate_cache(&cache, &guard) {
        eprintln!("SKIP: artifacts not present");
        return;
    }
    let mut adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();
    let fixture = load_fixture();
    let title = fixture["long_document"]["title"].as_str().unwrap();
    let fragment = fixture["long_document"]["fragment"].as_str().unwrap();

    let units = adapter.chunk_text(title, fragment);
    let spans = adapter.chunk_passage(title, fragment);
    assert_eq!(units.len(), spans.len(), "one unit per derived chunk");
    assert!(units.len() > 1, "reference long document must chunk");
    let shift = title.len() as u64 + 1;
    for (u, s) in units.iter().zip(spans.iter()) {
        assert_eq!(u.text, format!("{title}\n{}", s.text));
        assert_eq!(
            (u.char_start, u.char_end),
            (shift + s.char_start as u64, shift + s.char_end as u64)
        );
    }

    // Document embedding through the seam equals the Passage-role recipe.
    // (Qualified syntax: the inherent role-taking `embed` shadows the
    // trait seam by name — the seam is the Passage-role projection.)
    let via_seam = ProjectorEmbedder::embed(&mut adapter, &units[0].text).unwrap();
    let via_recipe = adapter.embed(&units[0].text, Role::Passage).unwrap();
    assert_eq!(via_seam, via_recipe.vector);
}

/// Query bridge: the mutex-guarded adapter serves the query role through
/// the search backend seam, with prefix-asymmetry evidence (Query !=
/// Passage vectors for the same text).
#[test]
fn query_bridge_uses_query_role() {
    let (cache, guard) = test_cache();
    if !populate_cache(&cache, &guard) {
        eprintln!("SKIP: artifacts not present");
        return;
    }
    let mut adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();
    let text = "how to configure fjall persistence";
    let expected_q = adapter.embed(text, Role::Query).unwrap().vector;
    let expected_p = adapter.embed(text, Role::Passage).unwrap().vector;
    assert_ne!(
        expected_q, expected_p,
        "prefix asymmetry must hold for the bridge to be meaningful"
    );

    let bridged = Mutex::new(adapter);
    let via_bridge = bridged.embed_query(text).unwrap();
    assert_eq!(
        via_bridge, expected_q,
        "query bridge must embed with the Query role"
    );
}
