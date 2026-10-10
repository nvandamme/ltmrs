//! Table maintenance tests (moved verbatim from `table.rs`).

use super::test_support::*;
use super::{SearchTable, batches_to_rows, search_schema};
use ltmrs_domain::id::ModelFingerprint;

/// Task 10: optimization must preserve row content and remain idempotent.
#[tokio::test]
async fn optimize_preserves_content_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    tbl.publish_rows(&[row(1, "rust async runtime", 0, None)])
        .await
        .unwrap();

    // Two optimization passes: both must succeed and keep the data intact.
    tbl.optimize().await.unwrap();
    tbl.optimize().await.unwrap();

    let count = tbl.count_rows(None).await.unwrap();
    assert_eq!(count, 1);
    let rows = tbl.rows_where("lexical_text LIKE '%rust%'").await.unwrap();
    assert_eq!(rows.len(), 1);
}

/// Task 10: version pruning must not remove the current version — a fresh
/// reader still sees all committed data.
#[tokio::test]
async fn prune_keeps_current_version_readable() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();
    tbl.publish_rows(&[row(1, "a", 0, None), row(2, "b", 0, None)])
        .await
        .unwrap();

    // Prune versions older than now (none qualify): current data must survive.
    tbl.prune_old_versions(0).await.unwrap();

    let count = tbl.count_rows(None).await.unwrap();
    assert_eq!(count, 2);
}

/// Task 11: a cached reader handle does not auto-refresh; after new commits
/// the reader must be refreshed (or reopened) to see them.
#[tokio::test]
async fn reader_must_refresh_after_new_commits() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap().to_string();

    // Writer publishes a row, then closes.
    {
        let writer = SearchTable::open(&uri).await.unwrap();
        writer
            .publish_rows(&[row(1, "first", 0, None)])
            .await
            .unwrap();
    }

    // A reader opens and sees one row.
    let mut reader = SearchTable::open(&uri).await.unwrap();
    assert_eq!(reader.count_rows(None).await.unwrap(), 1);

    // Another writer commits a second row while the reader is open.
    {
        let writer = SearchTable::open(&uri).await.unwrap();
        writer
            .publish_rows(&[row(2, "second", 0, None)])
            .await
            .unwrap();
    }

    // The cached handle does not see it until refreshed (task 11 contract:
    // do not rely on a cached handle being automatically current).
    assert_eq!(reader.count_rows(None).await.unwrap(), 1);

    reader.refresh().await.unwrap();
    assert_eq!(reader.count_rows(None).await.unwrap(), 2);
}

/// Task 11: reopening the table after commits and generation changes always
/// yields the latest committed state.
#[tokio::test]
async fn reopen_sees_latest_committed_state() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap().to_string();

    // Publish under two different model fingerprints (a "generation change").
    {
        let writer = SearchTable::open(&uri).await.unwrap();
        let mut r1 = row(1, "gen1", 0, None);
        r1.model_fingerprint = ModelFingerprint::new(1);
        let mut r2 = row(1, "gen2", 0, None);
        r2.model_fingerprint = ModelFingerprint::new(2);
        writer
            .publish_rows(&[r1.clone(), r2.clone()])
            .await
            .unwrap();
    }

    // A fresh reader sees both generations.
    let reader = SearchTable::open(&uri).await.unwrap();
    assert_eq!(reader.count_rows(None).await.unwrap(), 2);

    // Filtered by generation: each fingerprint is queryable independently.
    let gen1 = reader.rows_where("model_fingerprint = 1").await.unwrap();
    assert_eq!(gen1.len(), 1);
    assert_eq!(gen1[0].lexical_text, "gen1");
}

/// Corrupt rows fail the read instead of panicking the daemon: a
/// malformed memory id errors with row identity, never unwraps.
#[test]
fn corrupt_rows_error_instead_of_panicking() {
    use lancedb::arrow::arrow_array::{
        Float64Array, RecordBatch, StringArray, UInt32Array, UInt64Array,
    };
    use std::sync::Arc;
    let dim = 2;
    let schema = search_schema(dim);
    let embedding_type = lancedb::arrow::arrow_schema::DataType::FixedSizeList(
        Arc::new(lancedb::arrow::arrow_schema::Field::new(
            "item",
            lancedb::arrow::arrow_schema::DataType::Float32,
            true,
        )),
        dim as i32,
    );
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from(vec![1])),
            Arc::new(StringArray::from(vec!["not-a-uuid"])),
            Arc::new(UInt64Array::from(vec![1])),
            Arc::new(UInt64Array::from(vec![1])),
            Arc::new(UInt32Array::from(vec![0])),
            Arc::new(StringArray::from(vec!["text"])),
            Arc::new(UInt64Array::from(vec![0])),
            Arc::new(UInt64Array::from(vec![1])),
            Arc::new(StringArray::from(vec![Option::<&str>::None])),
            Arc::new(StringArray::from(vec!["fact"])),
            Arc::new(UInt64Array::from(vec![0])),
            Arc::new(UInt64Array::from(vec![0])),
            lancedb::arrow::arrow_array::new_null_array(&embedding_type, 1),
            Arc::new(StringArray::from(vec!["v1"])),
            Arc::new(Float64Array::from(vec![0.5])),
        ],
    )
    .unwrap();
    let err = batches_to_rows(&[batch]).unwrap_err();
    assert!(
        err.message.contains("memory_id"),
        "must identify the corrupt column, got: {err:?}"
    );
}

/// Pre-confidence tables fail loudly: a batch without the appended
/// confidence column errors naming the missing column (rebuild
/// directive) instead of defaulting confidence or misaligning fields.
#[test]
fn pre_confidence_schema_errors_naming_confidence() {
    use lancedb::arrow::arrow_array::{RecordBatch, StringArray, UInt32Array, UInt64Array};
    use std::sync::Arc;
    let dim = 2;
    let full = search_schema(dim);
    // All columns except the trailing confidence one.
    let fields: Vec<_> = full.fields()[..full.fields().len() - 1].to_vec();
    let schema = Arc::new(lancedb::arrow::arrow_schema::Schema::new(fields));
    let embedding_type = lancedb::arrow::arrow_schema::DataType::FixedSizeList(
        Arc::new(lancedb::arrow::arrow_schema::Field::new(
            "item",
            lancedb::arrow::arrow_schema::DataType::Float32,
            true,
        )),
        dim as i32,
    );
    let id = "12345678-1234-1234-1234-123456789012";
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from(vec![1])),
            Arc::new(StringArray::from(vec![id])),
            Arc::new(UInt64Array::from(vec![1])),
            Arc::new(UInt64Array::from(vec![1])),
            Arc::new(UInt32Array::from(vec![0])),
            Arc::new(StringArray::from(vec!["text"])),
            Arc::new(UInt64Array::from(vec![0])),
            Arc::new(UInt64Array::from(vec![1])),
            Arc::new(StringArray::from(vec![Option::<&str>::None])),
            Arc::new(StringArray::from(vec!["fact"])),
            Arc::new(UInt64Array::from(vec![0])),
            Arc::new(UInt64Array::from(vec![0])),
            lancedb::arrow::arrow_array::new_null_array(&embedding_type, 1),
            Arc::new(StringArray::from(vec!["v1"])),
        ],
    )
    .unwrap();
    let err = batches_to_rows(&[batch]).unwrap_err();
    assert!(
        err.message.contains("confidence"),
        "must name the missing column, got: {err:?}"
    );
}

/// Column resolution is by name, not position: a reordered schema (as
/// produced by appended `_distance`/`_score` extras or upgrades) parses
/// to identical rows instead of silently misattributing fields.
#[test]
fn reordered_columns_parse_by_name() {
    use lancedb::arrow::arrow_array::{RecordBatch, StringArray, UInt32Array, UInt64Array};
    use std::sync::Arc;
    // Same fields as search_schema(2) with lexical_text and fragment_type
    // swapped in position.
    let fields = vec![
        lancedb::arrow::arrow_schema::Field::new(
            "store_generation",
            lancedb::arrow::arrow_schema::DataType::UInt64,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "memory_id",
            lancedb::arrow::arrow_schema::DataType::Utf8,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "document_revision",
            lancedb::arrow::arrow_schema::DataType::UInt64,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "model_fingerprint",
            lancedb::arrow::arrow_schema::DataType::UInt64,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "chunk_id",
            lancedb::arrow::arrow_schema::DataType::UInt32,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "fragment_type",
            lancedb::arrow::arrow_schema::DataType::Utf8,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "char_start",
            lancedb::arrow::arrow_schema::DataType::UInt64,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "char_end",
            lancedb::arrow::arrow_schema::DataType::UInt64,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "project",
            lancedb::arrow::arrow_schema::DataType::Utf8,
            true,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "lexical_text",
            lancedb::arrow::arrow_schema::DataType::Utf8,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "created_at_millis",
            lancedb::arrow::arrow_schema::DataType::UInt64,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "updated_at_millis",
            lancedb::arrow::arrow_schema::DataType::UInt64,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "embedding",
            lancedb::arrow::arrow_schema::DataType::FixedSizeList(
                Arc::new(lancedb::arrow::arrow_schema::Field::new(
                    "item",
                    lancedb::arrow::arrow_schema::DataType::Float32,
                    true,
                )),
                2,
            ),
            true,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "chunker_version",
            lancedb::arrow::arrow_schema::DataType::Utf8,
            false,
        ),
        lancedb::arrow::arrow_schema::Field::new(
            "confidence",
            lancedb::arrow::arrow_schema::DataType::Float64,
            false,
        ),
    ];
    let schema = Arc::new(lancedb::arrow::arrow_schema::Schema::new(fields));
    let id = "12345678-1234-1234-1234-123456789012";
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from(vec![7])),
            Arc::new(StringArray::from(vec![id])),
            Arc::new(UInt64Array::from(vec![3])),
            Arc::new(UInt64Array::from(vec![5])),
            Arc::new(UInt32Array::from(vec![0])),
            Arc::new(StringArray::from(vec!["fact"])),
            Arc::new(UInt64Array::from(vec![0])),
            Arc::new(UInt64Array::from(vec![9])),
            Arc::new(StringArray::from(vec![Option::<&str>::None])),
            Arc::new(StringArray::from(vec!["hello world"])),
            Arc::new(UInt64Array::from(vec![100])),
            Arc::new(UInt64Array::from(vec![200])),
            lancedb::arrow::arrow_array::new_null_array(
                &lancedb::arrow::arrow_schema::DataType::FixedSizeList(
                    Arc::new(lancedb::arrow::arrow_schema::Field::new(
                        "item",
                        lancedb::arrow::arrow_schema::DataType::Float32,
                        true,
                    )),
                    2,
                ),
                1,
            ),
            Arc::new(StringArray::from(vec!["v1"])),
            Arc::new(lancedb::arrow::arrow_array::Float64Array::from(vec![0.5])),
        ],
    )
    .unwrap();
    let rows = batches_to_rows(&[batch]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].lexical_text, "hello world");
    assert_eq!(rows[0].fragment_type, "fact");
    assert_eq!(rows[0].store_generation.as_u64(), 7);
    assert_eq!(rows[0].created_at_millis, 100);
}
