//! Lance table writes: publish, delete (moved verbatim from `table.rs`).

use super::{SearchTable, row_batch};
use lancedb::arrow::arrow_array::RecordBatchIterator;

use crate::search::row::SearchRow;
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};

impl SearchTable {
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

    /// Delete all rows matching a DataFusion filter predicate (tombstone / delete
    /// propagation, task 7).
    pub async fn delete_where(&self, filter: &str) -> DomainResult<()> {
        self.table
            .delete(filter)
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(())
    }
}
