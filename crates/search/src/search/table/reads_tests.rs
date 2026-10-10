//! Table read-path tests (moved verbatim from `table.rs`).

use std::sync::Arc;

use super::test_support::*;
use super::{EMBEDDING_DIM, SEARCH_TABLE, SearchTable, search_schema};
use ltmrs_domain::id::ChunkId;

#[tokio::test]
async fn publish_and_read_back_row() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    tbl.publish_rows(&[row(1, "hello world", 0, None)])
        .await
        .unwrap();

    let rows = tbl.rows_where("lexical_text LIKE '%hello%'").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].memory_id, eid(1));
    assert_eq!(rows[0].document_revision.as_u64(), 0);
}

/// The chunker version round-trips per row so policy changes stay
/// attributable (never silently mixed under one fingerprint).
#[tokio::test]
async fn chunker_version_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    let mut a = row(1, "alpha text", 0, None);
    a.chunker_version = "e5-chunks-v1".to_string();
    let mut b = row(2, "beta text", 0, None);
    b.chunker_version = "single-chunk-v1".to_string();
    tbl.publish_rows(&[a, b]).await.unwrap();

    let e5 = tbl
        .rows_where("chunker_version = 'e5-chunks-v1'")
        .await
        .unwrap();
    assert_eq!(e5.len(), 1);
    assert_eq!(e5[0].memory_id, eid(1));
    let single = tbl
        .rows_where("chunker_version = 'single-chunk-v1'")
        .await
        .unwrap();
    assert_eq!(single.len(), 1);
    assert_eq!(single[0].memory_id, eid(2));
}

/// FTS freshness contract: does the index cover rows appended AFTER
/// creation? The production rebuild policy depends on the answer.
#[tokio::test]
async fn fts_index_covers_rows_appended_after_creation() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    tbl.publish_rows(&[row(1, "alpha onlyonce", 0, None)])
        .await
        .unwrap();
    tbl.create_fts_index().await.unwrap();
    tbl.publish_rows(&[row(2, "beta onlytwice", 0, None)])
        .await
        .unwrap();
    let hits = tbl.fts_query("onlytwice", 10, None).await.unwrap();
    assert_eq!(hits.len(), 1, "FTS must cover post-creation rows");
    assert_eq!(hits[0].memory_id, eid(2));
}

/// FTS rebuild contract: creating the index twice must succeed (the
/// production policy rebuilds after new writes).
#[tokio::test]
async fn fts_index_recreation_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    tbl.publish_rows(&[row(1, "alpha onlyonce", 0, None)])
        .await
        .unwrap();
    tbl.create_fts_index().await.unwrap();
    tbl.create_fts_index().await.unwrap();
    let hits = tbl.fts_query("onlyonce", 10, None).await.unwrap();
    assert_eq!(hits.len(), 1);
}

/// Idempotent ensure: builds the FTS index on first call (true),
/// skips when already present (false) — never a wasteful rebuild.
#[tokio::test]
async fn ensure_fts_index_builds_once() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    assert!(!tbl.fts_index_ready().await.unwrap());
    assert!(tbl.ensure_fts_index().await.unwrap(), "first call builds");
    assert!(tbl.fts_index_ready().await.unwrap());
    assert!(!tbl.ensure_fts_index().await.unwrap(), "second call skips");
}

/// Cross-handle reality: index metadata is snapshot-pinned (a live
/// handle does NOT observe a new index), but `refresh()` reopens to
/// the latest version and observes it. Production serving refreshes
/// per request; this pins the contract it relies on.
#[tokio::test]
async fn fts_index_visible_across_handles_after_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let a = SearchTable::open(uri).await.unwrap();
    let mut b = SearchTable::open(uri).await.unwrap();
    a.publish_rows(&[row(1, "alpha onlyonce", 0, None)])
        .await
        .unwrap();
    a.create_fts_index().await.unwrap();
    assert!(
        !b.fts_index_ready().await.unwrap(),
        "handles are snapshot-pinned"
    );
    b.refresh().await.unwrap();
    assert!(
        b.fts_index_ready().await.unwrap(),
        "refresh observes the new index"
    );
    let hits = b.fts_query("onlyonce", 10, None).await.unwrap();
    assert_eq!(hits.len(), 1);
}

/// The version column is appended last: positional reads depend on it,
/// so its slot is pinned here, not just its name.
#[test]
fn schema_appends_chunker_version_last() {
    let schema = search_schema(EMBEDDING_DIM);
    assert_eq!(schema.fields().len(), 15);
    assert_eq!(schema.fields()[13].name(), "chunker_version");
    assert_eq!(schema.fields()[14].name(), "confidence");
}

/// Same-revision policy migration converges: republishing a revision
/// under a new chunker version purges the old policy's chunk ids instead
/// of lingering mixed-policy rows as current (RQ-08).
#[tokio::test]
async fn same_revision_policy_change_purges_old_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    // Revision 0 under policy A with 3 chunks.
    let mut set_a = Vec::new();
    for c in 0..3u32 {
        let mut r = row(1, &format!("chunk {c} text"), 0, None);
        r.chunk_id = ChunkId::new(c);
        r.chunker_version = "policy-a-v1".to_string();
        set_a.push(r);
    }
    tbl.publish_rows(&set_a).await.unwrap();
    assert_eq!(tbl.count_rows(None).await.unwrap(), 3);
    // Same revision under policy B with 2 chunks.
    let mut set_b = Vec::new();
    for c in 0..2u32 {
        let mut r = row(1, &format!("chunk {c} text"), 0, None);
        r.chunk_id = ChunkId::new(c);
        r.chunker_version = "policy-b-v1".to_string();
        set_b.push(r);
    }
    tbl.publish_rows(&set_b).await.unwrap();
    // Exactly the new policy's chunk set survives.
    assert_eq!(tbl.count_rows(None).await.unwrap(), 2);
    let kept = tbl
        .rows_where("chunker_version = 'policy-b-v1'")
        .await
        .unwrap();
    assert_eq!(kept.len(), 2);
    let stale = tbl
        .rows_where("chunker_version = 'policy-a-v1'")
        .await
        .unwrap();
    assert!(
        stale.is_empty(),
        "old-policy chunks must not linger as current"
    );
}

/// Opening a table that predates confidence pre-filtering fails fast
/// with a rebuild directive instead of misreading shifted columns.
#[tokio::test]
async fn open_rejects_table_without_confidence() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap().to_string();
    // Craft a pre-confidence table: current schema minus the last column.
    let full = search_schema(EMBEDDING_DIM);
    let old = Arc::new(lancedb::arrow::arrow_schema::Schema::new(
        full.fields()[..full.fields().len() - 1].to_vec(),
    ));
    let db = lancedb::connect(&uri).execute().await.unwrap();
    db.create_empty_table(SEARCH_TABLE, old)
        .mode(lancedb::database::CreateTableMode::exist_ok(|req| req))
        .execute()
        .await
        .unwrap();
    drop(db);

    let err = match SearchTable::open(&uri).await {
        Ok(_) => panic!("opening a pre-confidence table must fail"),
        Err(e) => e,
    };
    assert!(
        err.message.contains("rebuild"),
        "must direct a rebuild, got: {}",
        err.message
    );
}

/// Opening a table that predates chunker versioning (missing the
/// chunker_version column itself) fails with the chunker-specific
/// rebuild directive.
#[tokio::test]
async fn open_rejects_table_without_chunker_version() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap().to_string();
    // Craft a pre-versioning table: columns up to (not incl.) embedding's
    // successor — no chunker_version, no confidence.
    let full = search_schema(EMBEDDING_DIM);
    let slot = full.index_of("chunker_version").unwrap();
    let old = Arc::new(lancedb::arrow::arrow_schema::Schema::new(
        full.fields()[..slot].to_vec(),
    ));
    let db = lancedb::connect(&uri).execute().await.unwrap();
    db.create_empty_table(SEARCH_TABLE, old)
        .mode(lancedb::database::CreateTableMode::exist_ok(|req| req))
        .execute()
        .await
        .unwrap();
    drop(db);

    let err = match SearchTable::open(&uri).await {
        Ok(_) => panic!("opening a pre-versioning table must fail"),
        Err(e) => e,
    };
    assert!(
        err.message.contains("chunker versioning"),
        "must name the missing chunker versioning, got: {}",
        err.message
    );
}

/// RQ-08: publishing a newer revision must remove the superseded row so an
/// obsolete revision is never recalled as current.
#[tokio::test]
async fn publish_newer_revision_removes_superseded_row() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();

    // Publish revision 0, then a newer revision 1 for the same memory.
    tbl.publish_rows(&[row(1, "old text", 0, None)])
        .await
        .unwrap();
    tbl.publish_rows(&[row(1, "new text", 1, None)])
        .await
        .unwrap();

    // Exactly one row remains: the current revision only.
    let count = tbl.count_rows(None).await.unwrap();
    assert_eq!(
        count, 1,
        "superseded revisions must not linger in the projection"
    );

    let rows = tbl.rows_where("lexical_text LIKE '%new%'").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].document_revision.as_u64(), 1);

    // The stale text is gone: a lexical query for it returns nothing.
    let stale = tbl.rows_where("lexical_text LIKE '%old%'").await.unwrap();
    assert!(stale.is_empty());
}

/// AD-06 probe: a lexical-ready row with a NULL vector must round-trip and
/// be filterable; the pinned Lance build either supports this or not.
#[tokio::test]
async fn null_vector_row_round_trips_and_filters() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    // 384 zeros matches the table dimension.
    tbl.publish_rows(&[row(1, "lexical only", 0, None)])
        .await
        .unwrap();

    let count = tbl.count_rows(Some("embedding IS NULL")).await.unwrap();
    assert_eq!(count, 1);

    let pred = format!("memory_id = '{}'", eid(1).as_uuid());
    let rows = tbl.rows_where(&pred).await.unwrap();
    assert!(rows[0].embedding.is_none());
}

#[tokio::test]
async fn vector_row_round_trips_with_embedding() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    let mut vec = vec![0.0f32; 384];
    vec[0] = 1.0;
    tbl.publish_rows(&[row(1, "with vector", 0, Some(vec))])
        .await
        .unwrap();

    let rows = tbl.rows_where("embedding IS NOT NULL").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].embedding.as_ref().unwrap()[0], 1.0);
}

/// Vector search never returns embedding-free rows: after a stalled
/// embedder, lexical-only rows must not inflate dense ranks.
#[tokio::test]
async fn vector_query_skips_null_vector_rows() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    let mut va = vec![0.0f32; 384];
    va[0] = 1.0;
    tbl.publish_rows(&[row(1, "alpha", 0, Some(va)), row(2, "beta", 0, None)])
        .await
        .unwrap();

    let hits = tbl
        .vector_query(&vec![1.0f32; 384], 10, None)
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "only the embedded row may hit, got {}",
        hits.len()
    );
    assert_eq!(hits[0].0.memory_id, eid(1));
}

/// Multi-row reads must return each row's OWN embedding (the child
/// buffer is batch-concatenated; row 0's slice must not leak into
/// every row or MMR diversity is computed on falsified vectors).
#[tokio::test]
async fn multi_row_reads_return_per_row_embeddings() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    let mut va = vec![0.0f32; 384];
    va[0] = 1.0;
    let mut vb = vec![0.0f32; 384];
    vb[1] = 1.0;
    tbl.publish_rows(&[row(1, "first", 0, Some(va)), row(2, "second", 0, Some(vb))])
        .await
        .unwrap();

    let rows = tbl.rows_where("embedding IS NOT NULL").await.unwrap();
    assert_eq!(rows.len(), 2);
    let by_text: std::collections::BTreeMap<&str, &[f32]> = rows
        .iter()
        .map(|r| {
            (
                r.lexical_text.as_str(),
                r.embedding.as_ref().unwrap().as_slice(),
            )
        })
        .collect();
    assert_eq!(by_text["first"][0], 1.0, "first row keeps its vector");
    assert_eq!(by_text["first"][1], 0.0, "first row keeps its vector");
    assert_eq!(by_text["second"][0], 0.0, "second row keeps its vector");
    assert_eq!(by_text["second"][1], 1.0, "second row keeps its vector");
}

#[tokio::test]
async fn publish_is_idempotent_on_same_identity() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    tbl.publish_rows(&[row(1, "v1", 0, None)]).await.unwrap();
    tbl.publish_rows(&[row(1, "v1", 0, None)]).await.unwrap();

    assert_eq!(tbl.count_rows(None).await.unwrap(), 1);
}

#[tokio::test]
async fn scalar_predicates_filter_by_generation_and_project() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    tbl.publish_rows(&[row(1, "a", 0, None), row(2, "b", 0, None)])
        .await
        .unwrap();

    let pred = format!(
        "memory_id = '{}' AND store_generation = 1",
        eid(1).as_uuid()
    );
    let count = tbl.count_rows(Some(&pred)).await.unwrap();
    assert_eq!(count, 1);
}

/// Task 3: an FTS index over lexical_text must make token queries return the
/// matching row (BM25), and a scalar BTree index on memory_id must speed up
/// equality predicates without changing results.
#[tokio::test]
async fn fts_index_returns_matching_row() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    tbl.publish_rows(&[
        row(1, "rust async runtime", 0, None),
        row(2, "python asyncio loop", 0, None),
        row(3, "go goroutines scheduling", 0, None),
    ])
    .await
    .unwrap();

    // Index must exist before full-text search can use it.
    tbl.create_fts_index().await.unwrap();

    let hits = tbl.fts_query("rust async", 10, None).await.unwrap();
    assert!(!hits.is_empty(), "expected at least one FTS hit");
    assert!(hits.iter().any(|r| r.memory_id == eid(1)));
}

#[tokio::test]
async fn scalar_index_preserves_predicate_results() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    tbl.publish_rows(&[row(1, "a", 0, None), row(2, "b", 0, None)])
        .await
        .unwrap();

    tbl.create_scalar_indexes().await.unwrap();

    let pred = format!("memory_id = '{}'", eid(2).as_uuid());
    let count = tbl.count_rows(Some(&pred)).await.unwrap();
    assert_eq!(count, 1);
}
