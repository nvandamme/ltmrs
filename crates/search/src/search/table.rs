//! Lance search projection table: publish/read of SearchRows (WP-05 tasks 2, 3).

use std::sync::Arc;

use lancedb::arrow::arrow_array::{RecordBatchIterator, StringArray, UInt64Array};
use lancedb::arrow::arrow_schema::{DataType, Field, SchemaRef};
use lancedb::connect;
use lancedb::database::CreateTableMode;
use lancedb::query::{ExecutableQuery, QueryBase};
use uuid::Uuid;

use crate::search::row::SearchRow;
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::{ChunkId, DocumentRevision, EntityId, ModelFingerprint, StoreGeneration};

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
/// `chunker_version` is appended last so all pre-existing column positions
/// stay stable; tables predating it are refused at open with a rebuild
/// directive (the projection is derived state, rebuilt from canonical).
pub fn search_schema(dim: u32) -> SchemaRef {
    Arc::new(lancedb::arrow::arrow_schema::Schema::new(vec![
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
        Field::new("chunker_version", DataType::Utf8, false),
        // Confidence rides last for the same positional-stability reason:
        // it enables pre-filtering at the source (RV-13), and tables
        // predating it rebuild from canonical like chunker_version did.
        Field::new("confidence", DataType::Float64, false),
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
        // Schema gate: tables predating chunker versioning would misalign
        // positional reads. The projection is derived state — rebuild it
        // from canonical (delete the table directory, re-project) rather
        // than misreading shifted columns.
        let live_schema = table
            .schema()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        require_chunker_version(&live_schema)?;
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
        // Published chunker versions per group (same-revision migration).
        let mut versions: std::collections::BTreeMap<(u64, String, u64), Vec<String>> =
            std::collections::BTreeMap::new();
        for row in rows {
            let key = (
                row.store_generation.as_u64(),
                row.memory_id.as_uuid().to_string(),
                row.model_fingerprint.as_u64(),
            );
            newest
                .entry(key.clone())
                .and_modify(|m| *m = (*m).max(row.document_revision.as_u64()))
                .or_insert_with(|| row.document_revision.as_u64());
            let vers = versions.entry(key).or_default();
            if !vers.contains(&row.chunker_version) {
                vers.push(row.chunker_version.clone());
            }
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

        // Drop superseded revisions for every group we just published, plus
        // same-revision rows from superseded chunking policies: a policy
        // change republishes the revision under a new version, and the old
        // policy's chunk ids must not linger as current (RQ-08).
        for ((generation, memory_id, fingerprint), max_rev) in &newest {
            let filter = format!(
                "store_generation = {} AND memory_id = '{}' AND model_fingerprint = {} AND document_revision < {}",
                generation, memory_id, fingerprint, max_rev
            );
            self.delete_where(&filter).await?;
            let kept_key = (*generation, memory_id.clone(), *fingerprint);
            let kept: Vec<String> = versions
                .get(&kept_key)
                .map(|vers| {
                    vers.iter()
                        .map(|v| format!("'{}'", crate::retrieval::scope::sql_quote(v)))
                        .collect()
                })
                .unwrap_or_default();
            if !kept.is_empty() {
                let filter = format!(
                    "store_generation = {} AND memory_id = '{}' AND model_fingerprint = {} AND document_revision = {} AND chunker_version NOT IN ({})",
                    generation,
                    memory_id,
                    fingerprint,
                    max_rev,
                    kept.join(",")
                );
                self.delete_where(&filter).await?;
            }
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
        let batches: Vec<lancedb::arrow::arrow_array::RecordBatch> = stream
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        batches_to_rows(&batches)
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
    /// `filter` optionally constrains results with a DataFusion predicate.
    pub async fn fts_query(
        &self,
        terms: &str,
        limit: usize,
        filter: Option<&str>,
    ) -> DomainResult<Vec<SearchRow>> {
        use lance_index::scalar::FullTextSearchQuery;

        // Check if FTS is available by attempting the query and handling the
        // specific error for missing INVERTED index gracefully.
        let mut builder = self
            .table
            .query()
            .full_text_search(FullTextSearchQuery::new(terms.to_string()));
        if let Some(f) = filter {
            builder = builder.only_if(f);
        }
        let stream = builder.limit(limit).execute().await;

        match stream {
            Ok(stream) => {
                use futures_util::TryStreamExt;
                let batches: Vec<lancedb::arrow::arrow_array::RecordBatch> = stream
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                batches_to_rows(&batches)
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

    /// Dense candidate query: nearest vectors to `vector` over the embedding
    /// column, optionally constrained by a DataFusion filter. Rows without a
    /// vector are never returned. The `_distance` column (cosine) is included
    /// in the returned batches for reference scoring.
    pub async fn vector_query(
        &self,
        vector: &[f32],
        limit: usize,
        filter: Option<&str>,
    ) -> DomainResult<Vec<(SearchRow, f32)>> {
        use lancedb::DistanceType;
        use lancedb::query::QueryBase;

        let mut builder = self
            .table
            .query()
            .nearest_to(vector)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        builder = builder.distance_type(DistanceType::Cosine);
        if let Some(f) = filter {
            builder = builder.only_if(f);
        }
        let stream = builder
            .limit(limit)
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        use futures_util::TryStreamExt;
        let batches: Vec<lancedb::arrow::arrow_array::RecordBatch> = stream
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        let mut out = Vec::new();
        for batch in &batches {
            if batch.num_rows() == 0 {
                continue;
            }
            let rows = batches_to_rows(std::slice::from_ref(batch))?;
            let dist = batch.column_by_name("_distance").and_then(|c| {
                c.as_any()
                    .downcast_ref::<lancedb::arrow::arrow_array::Float32Array>()
            });
            for (i, row) in rows.iter().enumerate() {
                let d = dist.map(|d| d.value(i)).unwrap_or(1.0);
                out.push((row.clone(), d));
            }
        }
        Ok(out)
    }

    /// Whether an FTS (INVERTED) index exists over the lexical_text column.
    /// Used for readiness reporting: without it, lexical recall degrades to
    /// empty results (graceful) and must be reported as not-ready.
    pub async fn fts_index_ready(&self) -> DomainResult<bool> {
        use lancedb::index::IndexType;
        let indices = self
            .table
            .list_indices()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(indices.iter().any(|c| {
            c.columns.iter().any(|p| p == "lexical_text") && c.index_type == IndexType::FTS
        }))
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

    /// Idempotent production ensure: builds the FTS index when absent
    /// (returns true), skips when present (false). The index covers rows
    /// appended after creation, so no rebuild watermark is needed — build
    /// once, serve forever. A build failure errors loudly for the
    /// caller's retry policy (derived state is always recoverable).
    pub async fn ensure_fts_index(&self) -> DomainResult<bool> {
        if self.fts_index_ready().await? {
            return Ok(false);
        }
        self.create_fts_index().await?;
        Ok(true)
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
    /// Re-applies the schema gate: a table swapped for a pre-versioning one
    /// under a live handle is refused rather than misread on next query.
    pub async fn refresh(&mut self) -> DomainResult<()> {
        let table = self
            .db
            .clone()
            .open_table(SEARCH_TABLE)
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let live_schema = table
            .schema()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        require_chunker_version(&live_schema)?;
        self.table = table;
        Ok(())
    }

    pub fn table(&self) -> &lancedb::table::Table {
        &self.table
    }
}

/// Schema gate shared by open and refresh: the chunker_version column must
/// exist at its exact positional slot with the exact type, because reads
/// are positional. Anything else risks silent column misalignment, so it is
/// refused with a rebuild directive (the projection is derived state).
fn require_chunker_version(schema: &SchemaRef) -> DomainResult<()> {
    let rebuild = || {
        DomainError::new(
            DomainErrorCode::Validation,
            "search table predates chunker versioning; rebuild the projection",
        )
    };
    let field = schema
        .field_with_name("chunker_version")
        .map_err(|_| rebuild())?;
    let at_slot = schema.index_of("chunker_version").map_err(|_| rebuild())?;
    if at_slot != 13 || field.data_type() != &DataType::Utf8 || field.is_nullable() {
        return Err(DomainError::new(
            DomainErrorCode::Validation,
            "search table has an incompatible chunker_version column; rebuild the projection",
        ));
    }
    let confidence = schema.field_with_name("confidence").map_err(|_| {
        DomainError::new(
            DomainErrorCode::Validation,
            "search table predates confidence pre-filtering; rebuild the projection",
        )
    })?;
    let at_slot = schema.index_of("confidence").map_err(|_| {
        DomainError::new(
            DomainErrorCode::Validation,
            "search table predates confidence pre-filtering; rebuild the projection",
        )
    })?;
    if at_slot != 14 || confidence.data_type() != &DataType::Float64 || confidence.is_nullable() {
        return Err(DomainError::new(
            DomainErrorCode::Validation,
            "search table has an incompatible confidence column; rebuild the projection",
        ));
    }
    Ok(())
}

fn row_batch(
    rows: &[SearchRow],
    schema: &SchemaRef,
) -> DomainResult<lancedb::arrow::arrow_array::RecordBatch> {
    use lancedb::arrow::arrow_array::{FixedSizeListArray, Float32Array, UInt32Array};
    use lancedb::arrow::arrow_buffer::NullBufferBuilder;

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

    lancedb::arrow::arrow_array::RecordBatch::try_new(
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
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.chunker_version.clone()),
            )),
            Arc::new(lancedb::arrow::arrow_array::Float64Array::from_iter_values(
                rows.iter().map(|r| r.confidence),
            )),
        ],
    )
    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
}

fn batches_to_rows(
    batches: &[lancedb::arrow::arrow_array::RecordBatch],
) -> DomainResult<Vec<SearchRow>> {
    use lancedb::arrow::arrow_array::{
        Array, FixedSizeListArray, Float32Array, Float64Array, UInt32Array,
    };
    let corrupt = |row: usize, column: &str| {
        DomainError::new(
            DomainErrorCode::Validation,
            format!("corrupt search row {row}: column {column} has an unexpected type or value"),
        )
    };
    let mut out = Vec::new();
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        // Resolve columns by name once per batch: positional reads silently
        // misalign when the schema evolves (appended _distance/_score today,
        // anything tomorrow). A missing column fails the read loudly.
        let schema = batch.schema();
        let col = |row: usize, name: &'static str| -> DomainResult<usize> {
            schema.index_of(name).map_err(|_| corrupt(row, name))
        };
        let c_store_generation = col(0, "store_generation")?;
        let c_memory_id = col(0, "memory_id")?;
        let c_document_revision = col(0, "document_revision")?;
        let c_model_fingerprint = col(0, "model_fingerprint")?;
        let c_chunk_id = col(0, "chunk_id")?;
        let c_lexical_text = col(0, "lexical_text")?;
        let c_char_start = col(0, "char_start")?;
        let c_char_end = col(0, "char_end")?;
        let c_project = col(0, "project")?;
        let c_fragment_type = col(0, "fragment_type")?;
        let c_created_at = col(0, "created_at_millis")?;
        let c_updated_at = col(0, "updated_at_millis")?;
        let c_embedding = col(0, "embedding")?;
        let c_chunker_version = col(0, "chunker_version")?;
        let c_confidence = col(0, "confidence")?;
        for i in 0..batch.num_rows() {
            let s = |idx: usize, name: &'static str| -> DomainResult<String> {
                Ok(batch
                    .column(idx)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| corrupt(i, name))?
                    .value(i)
                    .to_string())
            };
            let u64c = |idx: usize, name: &'static str| -> DomainResult<u64> {
                Ok(batch
                    .column(idx)
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or_else(|| corrupt(i, name))?
                    .value(i))
            };

            let embedding = match batch.column(c_embedding).data_type() {
                DataType::FixedSizeList(_, size) => {
                    let list = batch
                        .column(c_embedding)
                        .as_any()
                        .downcast_ref::<FixedSizeListArray>()
                        .ok_or_else(|| corrupt(i, "embedding"))?;
                    if list.is_null(i) {
                        None
                    } else {
                        // The child buffer concatenates every row: offset by
                        // this row's start, or every row reads row 0's slice.
                        let flat = Float32Array::from(list.values().to_data());
                        let base = list.value_offset(i) as usize;
                        Some((0..*size as usize).map(|k| flat.value(base + k)).collect())
                    }
                }
                _ => None,
            };

            let memory_id = EntityId::new(
                Uuid::parse_str(&s(c_memory_id, "memory_id")?)
                    .map_err(|_| corrupt(i, "memory_id"))?,
            );
            let project = if batch
                .column(c_project)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| corrupt(i, "project"))?
                .is_null(i)
            {
                None
            } else {
                Some(s(c_project, "project")?)
            };
            out.push(SearchRow {
                store_generation: StoreGeneration::new(u64c(
                    c_store_generation,
                    "store_generation",
                )?),
                memory_id,
                document_revision: DocumentRevision::new(u64c(
                    c_document_revision,
                    "document_revision",
                )?),
                model_fingerprint: ModelFingerprint::new(u64c(
                    c_model_fingerprint,
                    "model_fingerprint",
                )?),
                chunk_id: ChunkId::new(
                    batch
                        .column(c_chunk_id)
                        .as_any()
                        .downcast_ref::<UInt32Array>()
                        .ok_or_else(|| corrupt(i, "chunk_id"))?
                        .value(i),
                ),
                lexical_text: s(c_lexical_text, "lexical_text")?,
                char_start: u64c(c_char_start, "char_start")?,
                char_end: u64c(c_char_end, "char_end")?,
                project,
                fragment_type: s(c_fragment_type, "fragment_type")?,
                created_at_millis: u64c(c_created_at, "created_at_millis")?,
                updated_at_millis: u64c(c_updated_at, "updated_at_millis")?,
                embedding,
                chunker_version: s(c_chunker_version, "chunker_version")?,
                confidence: batch
                    .column(c_confidence)
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .ok_or_else(|| corrupt(i, "confidence"))?
                    .value(i),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltmrs_domain::id::{
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
            chunker_version: "single-chunk-v1".to_string(),
            lexical_text: text.to_string(),
            char_start: 0,
            char_end: text.len() as u64,
            project: Some("ltmrs".into()),
            fragment_type: "fact".into(),
            created_at_millis: 1000,
            confidence: 0.5,
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
}
