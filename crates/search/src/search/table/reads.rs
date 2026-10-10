//! Lance table reads: open, queries, versions (moved verbatim from `table.rs`).

use super::{
    EMBEDDING_DIM, SEARCH_TABLE, SearchTable, batches_to_rows, require_chunker_version,
    search_schema,
};
use lancedb::connect;
use lancedb::database::CreateTableMode;
use lancedb::query::{ExecutableQuery, QueryBase};

use crate::search::row::SearchRow;
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};

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
