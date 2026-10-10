//! Lance table maintenance: indexes, optimize, prune (moved verbatim from `table.rs`).

use super::{MaintenanceBudget, SearchTable};

use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};

impl SearchTable {
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
}
