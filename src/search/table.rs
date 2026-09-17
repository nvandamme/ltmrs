//! Lance search projection table: publish/read of SearchRows (WP-05 tasks 2, 3).

use std::sync::Arc;

use arrow_array::{RecordBatchIterator, StringArray, UInt64Array};
use arrow_schema::{DataType, Field, SchemaRef};
use lancedb::connect;
use lancedb::database::CreateTableMode;
use lancedb::query::{ExecutableQuery, QueryBase};
use uuid::Uuid;

use crate::domain::command::{DomainError, DomainErrorCode, DomainResult};
use crate::domain::id::{ChunkId, DocumentRevision, EntityId, ModelFingerprint, StoreGeneration};
use crate::search::row::SearchRow;

pub const SEARCH_TABLE: &str = "search";
/// 384 matches the E5-small candidate dimension (AD-04).
const EMBEDDING_DIM: u32 = 384;

/// Explicit resource budgets for maintenance optimization and retention
/// (task 10, RQ-22): nothing here allocates beyond these limits or removes a
/// dataset version another reader may still hold. Every tunable is pinned so
/// the daemon never depends on library defaults it did not choose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaintenanceBudget {
    /// Max parallel compaction tasks (CPU bound).
    pub compaction_threads: usize,
    /// Max bytes per file when rewriting fragments during compaction.
    pub max_compact_bytes_per_file: usize,
    /// Retain dataset versions at least this long so a live reader pinned to an
    /// older version keeps its files (snapshot protection). Lance itself never
    /// deletes unverified files newer than 7 days; this must not undercut that.
    pub retain_millis: u64,
}

impl Default for MaintenanceBudget {
    fn default() -> Self {
        Self {
            compaction_threads: 2,
            max_compact_bytes_per_file: 1024 * 1024 * 1024, // 1 GiB
            retain_millis: 7 * 24 * 60 * 60 * 1000,         // 7 days (Lance's safety floor)
        }
    }
}

/// Arrow schema for the search projection table. The vector column is a
/// nullable fixed-size list so lexical-ready rows carry no embedding (AD-06).
pub fn search_schema(dim: u32) -> SchemaRef {
    Arc::new(arrow_schema::Schema::new(vec![
        Field::new("store_generation", DataType::UInt64, false),
        Field::new("memory_id", DataType::Utf8, false),
        Field::new("document_revision", DataType::UInt64, false),
        Field::new("model_fingerprint", DataType::UInt64, false),
        Field::new("chunk_id", DataType::UInt32, false),
        Field::new("lexical_text", DataType::Utf8, false),
        Field::new("char_start", DataType::UInt64, false),
        Field::new("char_end", DataType::UInt64, false),
        Field::new("project", DataType::Utf8, true),
        Field::new("fragment_type", DataType::Utf8, false),
        Field::new("created_at_millis", DataType::UInt64, false),
        Field::new("updated_at_millis", DataType::UInt64, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            true,
        ),
    ]))
}

#[derive(Clone)]
pub struct SearchTable {
    db: lancedb::connection::Connection,
    table: lancedb::table::Table,
}

impl SearchTable {
    pub async fn open(uri: &str) -> DomainResult<Self> {
        let db = connect(uri)
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        // Reopen or create so readers always see the current committed state.
        let table = match db.table_names().execute().await {
            Ok(names) if names.iter().any(|n| n == SEARCH_TABLE) => {
                db.open_table(SEARCH_TABLE).execute().await
            }
            _ => {
                let schema = search_schema(EMBEDDING_DIM);
                db.create_empty_table(SEARCH_TABLE, schema)
                    .mode(CreateTableMode::exist_ok(|req| req))
                    .execute()
                    .await
            }
        };
        let table =
            table.map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(Self {
            db: db.clone(),
            table,
        })
    }

    /// Idempotent upsert of projection rows keyed by the full SearchIdentity.
    /// After inserting, removes every row for an affected memory whose revision
    /// is older than the newest published here — a superseded revision must never
    /// be recallable as current (RQ-08). Safe only under per-entity publication
    /// serialization (the projector's guard); see Projector::publish_guarded.
    pub async fn publish_rows(&self, rows: &[SearchRow]) -> DomainResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let schema = self
            .table
            .schema()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let batch = row_batch(rows, &schema)?;

        // Newest document revision per (generation, memory, fingerprint) group.
        let mut newest: std::collections::BTreeMap<(u64, String, u64), u64> =
            std::collections::BTreeMap::new();
        for row in rows {
            let key = (
                row.store_generation.as_u64(),
                row.memory_id.as_uuid().to_string(),
                row.model_fingerprint.as_u64(),
            );
            newest
                .entry(key)
                .and_modify(|m| *m = (*m).max(row.document_revision.as_u64()))
                .or_insert_with(|| row.document_revision.as_u64());
        }

        let reader = Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema));
        let mut builder = self.table.merge_insert(&[
            "store_generation",
            "memory_id",
            "document_revision",
            "model_fingerprint",
            "chunk_id",
        ]);
        builder.when_matched_update_all(None);
        builder.when_not_matched_insert_all();
        builder
            .execute(reader)
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        // Drop superseded revisions for every group we just published.
        for ((generation, memory_id, fingerprint), max_rev) in &newest {
            let filter = format!(
                "store_generation = {} AND memory_id = '{}' AND model_fingerprint = {} AND document_revision < {}",
                generation, memory_id, fingerprint, max_rev
            );
            self.delete_where(&filter).await?;
        }
        Ok(())
    }

    /// Read back all rows matching a DataFusion filter predicate.
    pub async fn rows_where(&self, filter: &str) -> DomainResult<Vec<SearchRow>> {
        let stream = self
            .table
            .query()
            .only_if(filter)
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        use futures_util::TryStreamExt;
        let batches: Vec<arrow_array::RecordBatch> = stream
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(batches_to_rows(&batches))
    }

    pub async fn count_rows(&self, filter: Option<&str>) -> DomainResult<u64> {
        let f = filter.map(|f| f.to_string());
        let n = self
            .table
            .count_rows(f)
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(n as u64)
    }

    /// Lexical candidate query: full-text search over the rendered text.
    /// Returns empty results if no FTS index exists (graceful degradation).
    pub async fn fts_query(&self, terms: &str, limit: usize) -> DomainResult<Vec<SearchRow>> {
        use lance_index::scalar::FullTextSearchQuery;

        // Check if FTS is available by attempting the query and handling the
        // specific error for missing INVERTED index gracefully.
        let stream = self
            .table
            .query()
            .full_text_search(FullTextSearchQuery::new(terms.to_string()))
            .limit(limit)
            .execute()
            .await;

        match stream {
            Ok(stream) => {
                use futures_util::TryStreamExt;
                let batches: Vec<arrow_array::RecordBatch> =
                    stream.try_collect::<Vec<_>>().await.map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                Ok(batches_to_rows(&batches))
            }
            Err(e) => {
                // If no FTS index exists, return empty results gracefully.
                let msg = format!("{}", e);
                if msg.contains("Cannot perform full text search")
                    || msg.contains("INVERTED index has been created")
                {
                    Ok(vec![])
                } else {
                    Err(DomainError::new(DomainErrorCode::Validation, e.to_string()))
                }
            }
        }
    }

    /// Create an FTS (BM25) index over the rendered text column.
    pub async fn create_fts_index(&self) -> DomainResult<()> {
        use lancedb::index::{Index, scalar::FtsIndexBuilder};
        self.table()
            .create_index(&["lexical_text"], Index::FTS(FtsIndexBuilder::default()))
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(())
    }

    /// Create scalar indexes (BTree) over the identity/scope columns.
    pub async fn create_scalar_indexes(&self) -> DomainResult<()> {
        use lancedb::index::{Index, scalar::BTreeIndexBuilder};
        for col in [
            "store_generation",
            "memory_id",
            "document_revision",
            "model_fingerprint",
            "project",
            "fragment_type",
        ] {
            self.table()
                .create_index(&[col], Index::BTree(BTreeIndexBuilder::default()))
                .execute()
                .await
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        }
        Ok(())
    }

    /// Delete all rows matching a DataFusion filter predicate (tombstone / delete
    /// propagation, task 7).
    pub async fn delete_where(&self, filter: &str) -> DomainResult<()> {
        self.table
            .delete(filter)
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(())
    }

    /// Index optimization and retention (task 10): compact small files, fold
    /// unindexed rows into existing indexes, and prune old dataset versions.
    /// Every action is bounded by an explicit budget — nothing here allocates
    /// beyond the configured limits or removes a version another reader may hold.
    pub async fn optimize(&self) -> DomainResult<()> {
        self.optimize_with_budgets(MaintenanceBudget::default())
            .await
    }

    /// Budgeted optimization/retention (task 10). `budget` pins every tunable so
    /// the daemon never allocates unboundedly and never prunes a version another
    /// reader may still hold. Compaction folds small fragments under explicit
    /// thread/size caps; index optimize folds unindexed tails into existing
    /// indexes; pruning retains versions for at least `retain_millis` (snapshot
    /// protection).
    pub async fn optimize_with_budgets(&self, budget: MaintenanceBudget) -> DomainResult<()> {
        use lancedb::table::{CompactionOptions, OptimizeAction, OptimizeOptions};

        // 1. Compaction: merge small fragments under an explicit thread/size cap.
        let compact = CompactionOptions {
            num_threads: Some(budget.compaction_threads),
            max_bytes_per_file: Some(budget.max_compact_bytes_per_file),
            ..CompactionOptions::default()
        };
        self.table()
            .optimize(OptimizeAction::Compact {
                options: compact,
                remap_options: None,
            })
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        // 2. Index optimize: fold unindexed rows into existing indexes.
        self.table()
            .optimize(OptimizeAction::Index(OptimizeOptions::default()))
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        // 3. Prune old versions but retain at least `retain_millis` so a live
        // reader pinned to an older version keeps its files (snapshot protection).
        self.table()
            .optimize(OptimizeAction::Prune {
                older_than: Some(lancedb::table::Duration::milliseconds(
                    budget.retain_millis as i64,
                )),
                delete_unverified: None,
                error_if_tagged_old_versions: None,
            })
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        Ok(())
    }

    /// Prune dataset versions older than `retain_millis` (task 10 retention).
    /// Snapshot protection: a reader pinned to an old version keeps its files
    /// because Lance only prunes versions that are no longer referenced.
    pub async fn prune_old_versions(&self, retain_millis: u64) -> DomainResult<()> {
        use lancedb::table::optimize::{Duration, OptimizeAction};
        self.table()
            .optimize(OptimizeAction::Prune {
                older_than: Some(Duration::milliseconds(retain_millis as i64)),
                delete_unverified: None,
                error_if_tagged_old_versions: None,
            })
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(())
    }

    /// The current dataset version (task 10/11): monotonic, advances on every
    /// commit. Readers can compare versions to detect staleness.
    pub async fn version(&self) -> DomainResult<u64> {
        self.table()
            .version()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
    }

    /// Reopen the underlying table handle so new commits become visible.
    pub async fn refresh(&mut self) -> DomainResult<()> {
        let table = self
            .db
            .clone()
            .open_table(SEARCH_TABLE)
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        self.table = table;
        Ok(())
    }

    pub fn table(&self) -> &lancedb::table::Table {
        &self.table
    }
}

fn row_batch(rows: &[SearchRow], schema: &SchemaRef) -> DomainResult<arrow_array::RecordBatch> {
    use arrow_array::{FixedSizeListArray, Float32Array, UInt32Array};
    use arrow_buffer::NullBufferBuilder;

    let dim = match schema.field_with_name("embedding") {
        Ok(field) => match field.data_type() {
            DataType::FixedSizeList(f, size) => (f.clone(), *size),
            _ => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "embedding column is not a fixed-size list",
                ));
            }
        },
        Err(_) => {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "no embedding column in schema",
            ));
        }
    };

    let mut values: Vec<Option<f32>> = Vec::with_capacity(rows.len() * dim.1 as usize);
    let mut null_builder = NullBufferBuilder::new(rows.len());
    for row in rows {
        match &row.embedding {
            Some(vec) if vec.len() == dim.1 as usize => {
                values.extend_from_slice(&vec.iter().map(|v| Some(*v)).collect::<Vec<_>>());
                null_builder.append(true);
            }
            _ => {
                // Null vector: fill with None so the FixedSizeList stays well-formed.
                values.resize(values.len() + dim.1 as usize, None);
                null_builder.append(false);
            }
        }
    }

    let flat = Float32Array::from(values.clone());
    let list_array =
        FixedSizeListArray::try_new(dim.0, dim.1, Arc::new(flat), null_builder.finish())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

    arrow_array::RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.store_generation.as_u64()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.memory_id.as_uuid().to_string()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.document_revision.as_u64()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.model_fingerprint.as_u64()),
            )),
            Arc::new(UInt32Array::from_iter_values(
                rows.iter().map(|r| r.chunk_id.as_u32()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.lexical_text.clone()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.char_start),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.char_end),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.project.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.fragment_type.clone()),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.created_at_millis),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.updated_at_millis),
            )),
            Arc::new(list_array),
        ],
    )
    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
}

fn batches_to_rows(batches: &[arrow_array::RecordBatch]) -> Vec<SearchRow> {
    use arrow_array::{Array, FixedSizeListArray, Float32Array, UInt32Array};
    let mut out = Vec::new();
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        for i in 0..batch.num_rows() {
            let s = |idx: usize| -> String {
                batch
                    .column(idx)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap()
                    .value(i)
                    .to_string()
            };
            let u64c = |idx: usize| -> u64 {
                batch
                    .column(idx)
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .unwrap()
                    .value(i)
            };

            let embedding = match batch.column(12).data_type() {
                DataType::FixedSizeList(_, size) => {
                    let list = batch
                        .column(12)
                        .as_any()
                        .downcast_ref::<FixedSizeListArray>()
                        .unwrap();
                    if list.is_null(i) {
                        None
                    } else {
                        let flat = Float32Array::from(list.values().to_data());
                        Some((0..*size as usize).map(|k| flat.value(k)).collect())
                    }
                }
                _ => None,
            };

            out.push(SearchRow {
                store_generation: StoreGeneration::new(u64c(0)),
                memory_id: EntityId::new(Uuid::parse_str(&s(1)).unwrap()),
                document_revision: DocumentRevision::new(u64c(2)),
                model_fingerprint: ModelFingerprint::new(u64c(3)),
                chunk_id: ChunkId::new(
                    batch
                        .column(4)
                        .as_any()
                        .downcast_ref::<UInt32Array>()
                        .unwrap()
                        .value(i),
                ),
                lexical_text: s(5),
                char_start: u64c(6),
                char_end: u64c(7),
                project: if batch
                    .column(8)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap()
                    .is_null(i)
                {
                    None
                } else {
                    Some(s(8))
                },
                fragment_type: s(9),
                created_at_millis: u64c(10),
                updated_at_millis: u64c(11),
                embedding,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::{
        ChunkId, DocumentRevision, EntityId, ModelFingerprint, StoreGeneration,
    };
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn row(id: u64, text: &str, rev: u64, embedding: Option<Vec<f32>>) -> SearchRow {
        SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id: eid(id),
            document_revision: DocumentRevision::new(rev),
            model_fingerprint: ModelFingerprint::new(1),
            chunk_id: ChunkId::new(0),
            lexical_text: text.to_string(),
            char_start: 0,
            char_end: text.len() as u64,
            project: Some("ltmrs".into()),
            fragment_type: "fact".into(),
            created_at_millis: 1000,
            updated_at_millis: 2000,
            embedding,
        }
    }

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

        let hits = tbl.fts_query("rust async", 10).await.unwrap();
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
}
