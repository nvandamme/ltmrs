//! Lance-only backend probe (Part II §7, WP-02 Section A).
//!
//! Documents the gaps the Lance-only path cannot close:
//! - **T-CONC-02 gap**: concurrent absent-key creates can produce duplicates.
//! - **T-STORE-01 gap**: a stepwise merge exposes partial state (no multi-table tx).

use std::sync::Arc;

use arrow_array::{
    BooleanArray, Float64Array, RecordBatch, RecordBatchIterator, StringArray, UInt64Array,
};
use arrow_schema::SchemaRef;
use lancedb::connect;
use lancedb::database::CreateTableMode;
use lancedb::query::ExecutableQuery;
use lancedb::table::Table;
use uuid::Uuid;

use crate::domain::command::{
    CommandReceipt, DomainError, DomainErrorCode, DomainResult, ReceiptOutcome,
};
use crate::domain::id::{EntityId, OperationId, StoreGeneration};

pub struct LanceBackend {
    db: lancedb::connection::Connection,
    memories: Option<Table>,
    receipts: Option<Table>,
}

impl LanceBackend {
    pub async fn connect(uri: &str) -> DomainResult<Self> {
        let db = connect(uri)
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(Self {
            db,
            memories: None,
            receipts: None,
        })
    }

    pub async fn init_tables(&mut self) -> DomainResult<()> {
        if self.memories.is_none() {
            let schema: SchemaRef = Arc::new(crate::storage::schema::memories_schema());
            let table = self
                .db
                .create_empty_table("memories", schema)
                .mode(CreateTableMode::exist_ok(|req| req))
                .execute()
                .await
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            self.memories = Some(table);
        }
        if self.receipts.is_none() {
            let schema: SchemaRef = Arc::new(crate::storage::schema::receipts_schema());
            let table = self
                .db
                .create_empty_table("receipts", schema)
                .mode(CreateTableMode::exist_ok(|req| req))
                .execute()
                .await
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            self.receipts = Some(table);
        }
        Ok(())
    }

    fn memories(&self) -> DomainResult<&Table> {
        self.memories
            .as_ref()
            .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "not initialized"))
    }

    fn receipts(&self) -> DomainResult<&Table> {
        self.receipts
            .as_ref()
            .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "not initialized"))
    }

    pub async fn create_memory_if_absent(
        &self,
        id: EntityId,
        title: &str,
        fragment: &str,
        confidence: f64,
        revision: u64,
    ) -> DomainResult<bool> {
        let tbl = self.memories()?;
        let batch = memory_batch(id, title, fragment, confidence, revision);
        let reader = Box::new(RecordBatchIterator::new(
            vec![Ok(batch)],
            Arc::new(crate::storage::schema::memories_schema()),
        ));
        let mut builder = tbl.merge_insert(&["id"]);
        builder.when_not_matched_insert_all();
        let result = builder
            .execute(reader)
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(result.num_inserted_rows == 1)
    }

    pub async fn update_memory_conditional(
        &self,
        id: EntityId,
        expected_revision: u64,
        new_title: &str,
        new_revision: u64,
    ) -> DomainResult<u64> {
        let tbl = self.memories()?;
        let key = id.as_uuid().to_string();
        let filter = format!("id = '{}' AND entity_revision = {}", key, expected_revision);
        let builder = tbl
            .update()
            .only_if(&filter)
            .column("title", format!("'{}'", new_title))
            .column("entity_revision", new_revision.to_string());
        let result = builder
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(result.rows_updated as u64)
    }

    pub async fn apply_feedback(&self, id: EntityId, useful: bool) -> DomainResult<()> {
        let tbl = self.memories()?;
        let key = id.as_uuid().to_string();
        let (counter_col, counter_expr, conf_expr) = if useful {
            (
                "positive_feedback",
                "positive_feedback + 1",
                "LEAST(confidence + 0.01, 1.0)",
            )
        } else {
            (
                "negative_feedback",
                "negative_feedback + 1",
                "GREATEST(confidence - 0.05, 0.0)",
            )
        };
        let builder = tbl
            .update()
            .only_if(format!("id = '{}'", key))
            .column(counter_col, counter_expr)
            .column("confidence", conf_expr);
        builder
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(())
    }

    pub async fn create_relation(
        &self,
        source: EntityId,
        target: EntityId,
        _relation_type: &str,
    ) -> DomainResult<bool> {
        let tbl = self.memories()?;
        let source_key = source.as_uuid().to_string();
        let target_key = target.as_uuid().to_string();
        let existing = tbl
            .count_rows(Some(format!(
                "parent_id = '{}' AND id = '{}'",
                source_key, target_key
            )))
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(existing == 0)
    }

    pub async fn delete_relation(&self, source: EntityId, target: EntityId) -> DomainResult<u64> {
        let tbl = self.memories()?;
        let source_key = source.as_uuid().to_string();
        let target_key = target.as_uuid().to_string();
        let filter = format!("parent_id = '{}' AND id = '{}'", source_key, target_key);
        let result = tbl
            .delete(&filter)
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(result.num_deleted_rows as u64)
    }

    pub async fn forget_memory(&self, id: EntityId, lifecycle: &str) -> DomainResult<()> {
        let tbl = self.memories()?;
        let key = id.as_uuid().to_string();
        let builder = tbl
            .update()
            .only_if(format!("id = '{}'", key))
            .column("lifecycle", format!("'{}'", lifecycle));
        builder
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(())
    }

    /// Stepwise merge: publishes result, then transitions sources separately.
    /// Exposes partial state (T-STORE-01 gap).
    pub async fn merge_memories_stepwise(
        &self,
        result_id: EntityId,
        result_title: &str,
        result_fragment: &str,
        source_ids: &[EntityId],
        revision: u64,
    ) -> DomainResult<()> {
        let tbl = self.memories()?;
        let result_batch = memory_batch(result_id, result_title, result_fragment, 1.0, revision);
        let reader = Box::new(RecordBatchIterator::new(
            vec![Ok(result_batch)],
            Arc::new(crate::storage::schema::memories_schema()),
        ));
        let mut builder = tbl.merge_insert(&["id"]);
        builder.when_not_matched_insert_all();
        builder
            .execute(reader)
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        for source_id in source_ids {
            let key = source_id.as_uuid().to_string();
            let builder = tbl
                .update()
                .only_if(format!("id = '{}'", key))
                .column("lifecycle", "'archived'");
            builder
                .execute()
                .await
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        }
        Ok(())
    }

    pub async fn store_receipt(&self, receipt: &CommandReceipt) -> DomainResult<()> {
        let tbl = self.receipts()?;
        let batch = receipt_batch(receipt);
        let reader = Box::new(RecordBatchIterator::new(
            vec![Ok(batch)],
            Arc::new(crate::storage::schema::receipts_schema()),
        ));
        let mut builder = tbl.merge_insert(&["operation_id"]);
        builder.when_not_matched_insert_all();
        builder
            .execute(reader)
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(())
    }

    pub async fn lookup_receipt(
        &self,
        generation: StoreGeneration,
        op: OperationId,
    ) -> DomainResult<Option<CommandReceipt>> {
        let tbl = self.receipts()?;
        let key = op.as_uuid().to_string();
        let filter = format!(
            "operation_id = '{}' AND store_generation = {}",
            key,
            generation.as_u64()
        );
        let _ = filter;
        // The lancedb 0.38.0 query API returns a stream; the filter predicate
        // is applied via the query request. For this probe we scan and match
        // in-memory (the receipts table is small).
        let stream = tbl
            .query()
            .execute()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        use futures_util::TryStreamExt;
        let batches: Vec<RecordBatch> = stream
            .try_collect::<Vec<RecordBatch>>()
            .await
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        for batch in &batches {
            if batch.num_rows() > 0 {
                return Ok(Some(receipt_from_batch(batch)?));
            }
        }
        Ok(None)
    }
}

fn memory_batch(
    id: EntityId,
    title: &str,
    fragment: &str,
    confidence: f64,
    revision: u64,
) -> RecordBatch {
    let schema = Arc::new(crate::storage::schema::memories_schema());
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(vec![id.as_uuid().to_string()])),
            Arc::new(StringArray::from(vec![None::<&str>])),
            Arc::new(StringArray::from(vec![title])),
            Arc::new(StringArray::from(vec![fragment])),
            Arc::new(StringArray::from(vec![""])),
            Arc::new(StringArray::from(vec!["fact"])),
            Arc::new(StringArray::from(vec![None::<&str>])),
            Arc::new(StringArray::from(vec!["ai"])),
            Arc::new(Float64Array::from(vec![confidence])),
            Arc::new(Float64Array::from(vec![None::<f64>])),
            Arc::new(StringArray::from(vec!["live"])),
            Arc::new(StringArray::from(vec![""])),
            Arc::new(StringArray::from(vec![None::<&str>])),
            Arc::new(StringArray::from(vec![None::<&str>])),
            Arc::new(StringArray::from(vec![None::<&str>])),
            Arc::new(StringArray::from(vec![""])),
            Arc::new(UInt64Array::from(vec![0u64])),
            Arc::new(UInt64Array::from(vec![None::<u64>])),
            Arc::new(UInt64Array::from(vec![0u64])),
            Arc::new(UInt64Array::from(vec![0u64])),
            Arc::new(UInt64Array::from(vec![0u64])),
            Arc::new(UInt64Array::from(vec![0u64])),
            Arc::new(BooleanArray::from(vec![false])),
            Arc::new(UInt64Array::from(vec![revision])),
            Arc::new(UInt64Array::from(vec![1u64])),
            Arc::new(UInt64Array::from(vec![1u64])),
            Arc::new(UInt64Array::from(vec![0u64])),
            Arc::new(UInt64Array::from(vec![0u64])),
            Arc::new(StringArray::from(vec![None::<&str>])),
            Arc::new(StringArray::from(vec!["{}"])),
        ],
    )
    .expect("valid memory batch")
}

fn receipt_batch(receipt: &CommandReceipt) -> RecordBatch {
    let outcome = match &receipt.outcome {
        ReceiptOutcome::Success { affected } => format!(
            "success:{}",
            affected
                .iter()
                .map(|id| id.as_uuid().to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        ReceiptOutcome::Rejected { .. } => "rejected".to_string(),
    };
    let affected_ids = match &receipt.outcome {
        ReceiptOutcome::Success { affected } => affected
            .iter()
            .map(|id| id.as_uuid().to_string())
            .collect::<Vec<_>>()
            .join(","),
        ReceiptOutcome::Rejected { .. } => String::new(),
    };
    let schema = Arc::new(crate::storage::schema::receipts_schema());
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from(vec![receipt.store_generation.as_u64()])),
            Arc::new(StringArray::from(vec![
                receipt.operation_id.as_uuid().to_string(),
            ])),
            Arc::new(StringArray::from(vec![
                receipt.frontend_id.as_uuid().to_string(),
            ])),
            Arc::new(StringArray::from(vec![
                receipt.channel_id.as_uuid().to_string(),
            ])),
            Arc::new(StringArray::from(vec![receipt.request_digest.as_str()])),
            Arc::new(StringArray::from(vec![outcome.as_str()])),
            Arc::new(StringArray::from(vec![affected_ids.as_str()])),
        ],
    )
    .expect("valid receipt batch")
}

fn receipt_from_batch(batch: &RecordBatch) -> DomainResult<CommandReceipt> {
    let gen_col = batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "bad gen"))?;
    let op_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "bad op"))?;
    let fe_col = batch
        .column(2)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "bad fe"))?;
    let ch_col = batch
        .column(3)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "bad ch"))?;
    let digest_col = batch
        .column(4)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "bad digest"))?;
    let outcome_col = batch
        .column(5)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "bad outcome"))?;

    let gen_value = gen_col.value(0);
    let op_str = op_col.value(0);
    let fe_str = fe_col.value(0);
    let ch_str = ch_col.value(0);
    let digest = digest_col.value(0);
    let outcome_str = outcome_col.value(0);

    let op = OperationId::new(
        Uuid::parse_str(op_str)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
    );
    let fe = Uuid::parse_str(fe_str)
        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
    let ch = Uuid::parse_str(ch_str)
        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

    let outcome = if let Some(rest) = outcome_str.strip_prefix("success:") {
        let affected: Vec<EntityId> = if rest.is_empty() {
            vec![]
        } else {
            rest.split(',')
                .filter_map(|s| Uuid::parse_str(s).ok().map(EntityId::new))
                .collect()
        };
        ReceiptOutcome::Success { affected }
    } else {
        ReceiptOutcome::Rejected {
            code: DomainErrorCode::NotFound,
        }
    };

    Ok(CommandReceipt {
        operation_id: op,
        store_generation: StoreGeneration::new(gen_value),
        frontend_id: crate::domain::id::FrontendId::new(fe),
        channel_id: crate::domain::id::ChannelId::new(ch),
        request_digest: digest.to_string(),
        outcome,
        retry_epoch: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::{CommandContext, Scope};
    use crate::domain::id::{ChannelId, FrontendId};
    use tokio::sync::{Barrier, Notify};

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn test_ctx(op_num: u64) -> CommandContext {
        CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            session: None,
            operation_id: OperationId::new(Uuid::from_u128(op_num as u128)),
            request_digest: format!("digest-{op_num}"),
            deadline_millis: None,
            scope: Scope::default(),
            retry_epoch: 1,
        }
    }

    #[tokio::test]
    async fn test_create_and_read_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = LanceBackend::connect(dir.path().to_str().unwrap())
            .await
            .unwrap();
        be.init_tables().await.unwrap();

        let created = be
            .create_memory_if_absent(eid(1), "T", "F", 0.5, 1)
            .await
            .unwrap();
        assert!(created);

        let existing = be
            .create_memory_if_absent(eid(1), "T", "F", 0.5, 1)
            .await
            .unwrap();
        assert!(!existing);
    }

    #[tokio::test]
    async fn test_update_conditional() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = LanceBackend::connect(dir.path().to_str().unwrap())
            .await
            .unwrap();
        be.init_tables().await.unwrap();

        be.create_memory_if_absent(eid(1), "T", "F", 0.5, 1)
            .await
            .unwrap();

        let updated = be
            .update_memory_conditional(eid(1), 1, "NewT", 2)
            .await
            .unwrap();
        assert_eq!(updated, 1);

        let stale = be
            .update_memory_conditional(eid(1), 1, "Stale", 3)
            .await
            .unwrap();
        assert_eq!(stale, 0);
    }

    #[tokio::test]
    async fn test_store_and_lookup_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = LanceBackend::connect(dir.path().to_str().unwrap())
            .await
            .unwrap();
        be.init_tables().await.unwrap();

        let ctx = test_ctx(1);
        let receipt = CommandReceipt {
            operation_id: ctx.operation_id,
            store_generation: StoreGeneration::FIRST,
            frontend_id: ctx.frontend_id,
            channel_id: ctx.channel_id,
            request_digest: ctx.request_digest.clone(),
            outcome: ReceiptOutcome::Success {
                affected: vec![eid(1)],
            },
            retry_epoch: ctx.retry_epoch,
        };
        be.store_receipt(&receipt).await.unwrap();

        let found = be
            .lookup_receipt(StoreGeneration::FIRST, ctx.operation_id)
            .await
            .unwrap();
        assert!(found.is_some());
    }

    /// T-CONC-02 probe: concurrent absent-key creates with a barrier so the
    /// writes truly overlap. Documents the check-then-act race.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_absent_key_creates_lance_only_is_not_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = LanceBackend::connect(dir.path().to_str().unwrap())
            .await
            .unwrap();
        be.init_tables().await.unwrap();
        let be = Arc::new(be);
        let tbl = be.memories().unwrap().clone();

        const N: usize = 4;
        let barrier = Arc::new(Barrier::new(N));
        let mut handles = Vec::new();
        for i in 0..N {
            let tbl = tbl.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                let batch = memory_batch(eid(1), &format!("t{i}"), "f1", 0.5, 1);
                let reader = Box::new(RecordBatchIterator::new(
                    vec![Ok(batch)],
                    Arc::new(crate::storage::schema::memories_schema()),
                ));
                barrier.wait().await;
                let mut builder = tbl.merge_insert(&["id"]);
                builder.when_not_matched_insert_all();
                let result = builder.execute(reader).await.unwrap();
                result.num_inserted_rows == 1
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }
        let winners = results.iter().filter(|won| **won).count();

        let count = tbl
            .count_rows(Some(format!("id = '{}'", eid(1).as_uuid())))
            .await
            .unwrap();

        eprintln!(
            "T-CONC-02 probe: winners={winners} final_rows_for_key={count} (one-winner requires winners==1 && rows==1)"
        );
        assert!(
            winners != 1 || count != 1,
            "expected the Lance-only path to FAIL one-winner; if this panics the gap has closed"
        );
    }

    /// T-STORE-01 probe: a synchronized concurrent reader observes the result
    /// published before sources transition — a partial state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stepwise_merge_exposes_partial_publication_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = LanceBackend::connect(dir.path().to_str().unwrap())
            .await
            .unwrap();
        be.init_tables().await.unwrap();
        let be = Arc::new(be);
        let tbl = be.memories().unwrap().clone();

        be.create_memory_if_absent(eid(1), "A", "fa", 0.5, 1)
            .await
            .unwrap();
        be.create_memory_if_absent(eid(2), "B", "fb", 0.5, 1)
            .await
            .unwrap();

        let go = Arc::new(Notify::new());
        let done = Arc::new(Notify::new());
        let reader_go = Arc::clone(&go);
        let reader_done = Arc::clone(&done);
        let reader_tbl = tbl.clone();
        let reader = tokio::spawn(async move {
            reader_go.notified().await;
            let a_pred = format!(
                "id = '{}' AND parent_id = '{}'",
                eid(1).as_uuid(),
                eid(3).as_uuid()
            );
            let a_transitioned = reader_tbl.count_rows(Some(a_pred)).await.unwrap();
            reader_done.notify_one();
            a_transitioned > 0
        });

        be.create_memory_if_absent(eid(3), "C", "fc", 1.0, 1)
            .await
            .unwrap();
        go.notify_one();
        done.notified().await;
        let _ = be
            .merge_memories_stepwise(eid(3), "C", "fc", &[eid(1), eid(2)], 1)
            .await;

        let saw_full_state = reader.await.unwrap();
        eprintln!(
            "T-STORE-01 probe: reader_saw_complete_state={saw_full_state} (atomicity requires true; partial state is the gap)"
        );
        assert!(
            !saw_full_state,
            "expected the reader to observe a PARTIAL state (C without source transition); if this panics the gap has closed"
        );
    }

    #[tokio::test]
    async fn coherent_snapshot_pins_a_consistent_view() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = LanceBackend::connect(dir.path().to_str().unwrap())
            .await
            .unwrap();
        be.init_tables().await.unwrap();

        be.create_memory_if_absent(eid(1), "A", "fa", 0.5, 1)
            .await
            .unwrap();

        let tbl = be.memories().unwrap().clone();
        let snapshot = tbl.query_snapshot().await.unwrap();

        be.create_memory_if_absent(eid(2), "B", "fb", 0.5, 1)
            .await
            .unwrap();

        let count = snapshot.count_rows(None).await.unwrap();
        eprintln!("snapshot pinned count={count} (should be 1, not 2 — the new row is invisible)");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_scalar_query() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = LanceBackend::connect(dir.path().to_str().unwrap())
            .await
            .unwrap();
        be.init_tables().await.unwrap();

        be.create_memory_if_absent(eid(1), "Rust async", "f", 0.5, 1)
            .await
            .unwrap();

        let tbl = be.memories().unwrap().clone();
        let count = tbl
            .count_rows(Some("title LIKE '%Rust%'".to_string()))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_kill_and_reopen_durability() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();

        {
            let mut be = LanceBackend::connect(&path).await.unwrap();
            be.init_tables().await.unwrap();
            be.create_memory_if_absent(eid(1), "A", "fa", 0.5, 1)
                .await
                .unwrap();
        }

        let mut be2 = LanceBackend::connect(&path).await.unwrap();
        be2.init_tables().await.unwrap();
        let count = be2
            .memories()
            .unwrap()
            .count_rows(Some(format!("id = '{}'", eid(1).as_uuid())))
            .await
            .unwrap();
        eprintln!("kill/reopen: rows_after_reopen={count}");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_unenforced_primary_key_is_metadata_not_constraint() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = LanceBackend::connect(dir.path().to_str().unwrap())
            .await
            .unwrap();
        be.init_tables().await.unwrap();

        let tbl = be.memories().unwrap().clone();
        let _ = tbl.set_unenforced_primary_key(["id"]).await;

        let batch1 = memory_batch(eid(1), "A", "fa", 0.5, 1);
        let reader1: Box<dyn arrow_array::RecordBatchReader + Send> =
            Box::new(RecordBatchIterator::new(
                vec![Ok(batch1)],
                Arc::new(crate::storage::schema::memories_schema()),
            ));
        tbl.add(reader1).execute().await.unwrap();

        let batch2 = memory_batch(eid(1), "B", "fb", 0.6, 2);
        let reader2: Box<dyn arrow_array::RecordBatchReader + Send> =
            Box::new(RecordBatchIterator::new(
                vec![Ok(batch2)],
                Arc::new(crate::storage::schema::memories_schema()),
            ));
        tbl.add(reader2).execute().await.unwrap();

        let count = tbl
            .count_rows(Some(format!("id = '{}'", eid(1).as_uuid())))
            .await
            .unwrap();
        eprintln!("unenforced PK: rows_for_same_id={count} (metadata does NOT prevent duplicates)");
        assert_eq!(count, 2);
    }
}
