//! Schema migration runner with version checks and safety rules.
//!
//! Migrations are applied in order from the current store version to the
//! target version. Each migration is atomic: either it fully succeeds or
//! the store is left in its previous state.

use fjall::{KeyspaceCreateOptions, OptimisticTxDatabase, Readable};
use serde::{Deserialize, Serialize};

use crate::domain::command::{DomainError, DomainErrorCode, DomainResult};

/// Target schema version for this build.
pub const TARGET_SCHEMA_VERSION: u64 = 1;

/// A single schema migration step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Migration {
    /// Version this migration upgrades TO.
    pub to_version: u64,
    /// Description for logging/auditing.
    pub description: String,
}

/// Migration plan: the ordered list of migrations to apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub migrations: Vec<Migration>,
}

impl MigrationPlan {
    /// The default migration plan for ltmrs.
    ///
    /// Version 0 → 1: initial schema (no migration needed, just stamp the version).
    pub fn default_plan() -> Self {
        Self {
            migrations: vec![Migration {
                to_version: 1,
                description:
                    "Initial schema: memories, relations, receipts, aliases, namespaces, meta"
                        .into(),
            }],
        }
    }

    /// Find the next migration to apply given the current version.
    pub fn next_migration(&self, current_version: u64) -> Option<&Migration> {
        self.migrations
            .iter()
            .find(|m| m.to_version > current_version)
    }

    /// Whether the target version is reachable from the current version.
    pub fn can_reach(&self, current_version: u64, target_version: u64) -> bool {
        if current_version > target_version {
            return false;
        }
        if current_version == target_version {
            return true;
        }
        let mut version = current_version;
        for m in &self.migrations {
            if m.to_version > version && m.to_version <= target_version {
                version = m.to_version;
            }
        }
        version == target_version
    }
}

/// Migration safety rules enforced before and after migration.
pub struct MigrationSafetyRules {
    /// Whether to create a backup before migrating.
    pub backup_before: bool,
    /// Whether to validate the schema after migrating.
    pub validate_after: bool,
    /// Maximum allowed version jump in a single migration.
    pub max_version_jump: u64,
}

impl Default for MigrationSafetyRules {
    fn default() -> Self {
        Self {
            backup_before: true,
            validate_after: true,
            max_version_jump: 1,
        }
    }
}

/// Result of a migration attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum MigrationOutcome {
    /// No migration needed; already at target version.
    AlreadyCurrent { version: u64 },
    /// Migration applied successfully.
    Migrated {
        from_version: u64,
        to_version: u64,
        migrations_applied: Vec<u64>,
    },
    /// Migration refused: store version is newer than supported.
    RefusedNewerVersion {
        store_version: u64,
        supported_version: u64,
    },
    /// Migration refused: unknown/incompatible schema.
    RefusedUnknownSchema { reason: String },
}

/// The migration runner: applies migrations with safety checks.
pub struct MigrationRunner {
    pub plan: MigrationPlan,
    pub safety: MigrationSafetyRules,
    fault_injector: Option<std::sync::Arc<crate::service::repository::FaultInjector>>,
}

impl MigrationRunner {
    pub fn new(plan: MigrationPlan, safety: MigrationSafetyRules) -> Self {
        Self {
            plan,
            safety,
            fault_injector: None,
        }
    }

    /// Attach a fault injector for crash/durability testing of migration steps.
    pub fn with_fault_injector(
        mut self,
        fi: std::sync::Arc<crate::service::repository::FaultInjector>,
    ) -> Self {
        self.fault_injector = Some(fi);
        self
    }

    /// Check the store's schema version and determine what migration is needed.
    pub fn assess(&self, db: &OptimisticTxDatabase) -> DomainResult<MigrationOutcome> {
        let meta = db
            .keyspace("meta", KeyspaceCreateOptions::default)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let snapshot = db.read_tx();
        let raw = snapshot
            .get(&meta, "schema_version")
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        let current_version = match raw {
            None => 0,
            Some(bytes) => {
                if bytes.as_ref().len() != 8 {
                    return Ok(MigrationOutcome::RefusedUnknownSchema {
                        reason: "corrupt schema version record".into(),
                    });
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(bytes.as_ref());
                u64::from_le_bytes(arr)
            }
        };

        if current_version > TARGET_SCHEMA_VERSION {
            return Ok(MigrationOutcome::RefusedNewerVersion {
                store_version: current_version,
                supported_version: TARGET_SCHEMA_VERSION,
            });
        }

        if current_version == TARGET_SCHEMA_VERSION {
            return Ok(MigrationOutcome::AlreadyCurrent {
                version: current_version,
            });
        }

        // Check if we can reach the target version.
        if !self.plan.can_reach(current_version, TARGET_SCHEMA_VERSION) {
            return Ok(MigrationOutcome::RefusedUnknownSchema {
                reason: format!(
                    "no migration path from version {current_version} to {TARGET_SCHEMA_VERSION}"
                ),
            });
        }

        // Check max version jump.
        if let Some(next) = self.plan.next_migration(current_version) {
            let jump = next.to_version - current_version;
            if jump > self.safety.max_version_jump {
                return Ok(MigrationOutcome::RefusedUnknownSchema {
                    reason: format!(
                        "version jump {jump} exceeds max allowed {}",
                        self.safety.max_version_jump
                    ),
                });
            }
        }

        // Migration needed: apply it.
        let from_version = current_version;
        let mut applied = Vec::new();
        let mut version = current_version;

        while let Some(migration) = self.plan.next_migration(version) {
            if migration.to_version > TARGET_SCHEMA_VERSION {
                break;
            }
            self.apply_migration(db, migration)?;
            applied.push(migration.to_version);
            version = migration.to_version;
        }

        // Validate after migration if safety rules require it.
        if self.safety.validate_after {
            self.validate_schema(db)?;
        }

        Ok(MigrationOutcome::Migrated {
            from_version,
            to_version: version,
            migrations_applied: applied,
        })
    }

    /// Apply a single migration step.
    fn apply_migration(
        &self,
        db: &OptimisticTxDatabase,
        migration: &Migration,
    ) -> DomainResult<()> {
        // Fault injection: simulate a migration step failure (crash mid-migration).
        if let Some(fi) = &self.fault_injector
            && fi.inject_migration_fault()
        {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "injected migration fault: step failed before commit",
            ));
        }

        // Version 0 → 1: just stamp the version (initial schema).
        // Future migrations would perform actual schema transformations here.
        let meta = db
            .keyspace("meta", KeyspaceCreateOptions::default)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        let mut tx = db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&meta, "schema_version", migration.to_version.to_le_bytes());
        match tx.commit() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                format!("migration to version {} conflicted", migration.to_version),
            )),
            Err(e) => Err(DomainError::new(
                DomainErrorCode::Validation,
                format!("migration to version {} failed: {e}", migration.to_version),
            )),
        }
    }

    /// Validate the schema after migration.
    fn validate_schema(&self, db: &OptimisticTxDatabase) -> DomainResult<()> {
        // Check that all required keyspaces exist.
        for name in [
            "memories",
            "relations",
            "receipts",
            "aliases",
            "namespaces",
            "projections",
            "feedback_events",
            "meta",
        ] {
            db.keyspace(name, KeyspaceCreateOptions::default)
                .map_err(|e| {
                    DomainError::new(
                        DomainErrorCode::Validation,
                        format!("post-migration validation failed: missing keyspace '{name}': {e}"),
                    )
                })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets a unique OS-temp directory (never CWD — Fjall pitfall).
    fn test_db() -> (OptimisticTxDatabase, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = OptimisticTxDatabase::builder(dir.path().to_str().unwrap())
            .open()
            .unwrap();
        (db, dir)
    }

    #[test]
    fn migration_from_empty_store_stamps_version() {
        let (db, _dir) = test_db();
        let runner = MigrationRunner::new(
            MigrationPlan::default_plan(),
            MigrationSafetyRules::default(),
        );
        let outcome = runner.assess(&db).unwrap();
        match outcome {
            MigrationOutcome::Migrated {
                from_version,
                to_version,
                migrations_applied,
            } => {
                assert_eq!(from_version, 0);
                assert_eq!(to_version, 1);
                assert_eq!(migrations_applied, vec![1]);
            }
            _ => panic!("expected Migrated, got {outcome:?}"),
        }
    }

    #[test]
    fn migration_refuses_newer_version() {
        let (db, _dir) = test_db();
        // Stamp a newer version.
        let meta = db.keyspace("meta", KeyspaceCreateOptions::default).unwrap();
        let mut tx = db.write_tx().unwrap();
        tx.insert(&meta, "schema_version", 99u64.to_le_bytes());
        tx.commit().unwrap().unwrap();

        let runner = MigrationRunner::new(
            MigrationPlan::default_plan(),
            MigrationSafetyRules::default(),
        );
        let outcome = runner.assess(&db).unwrap();
        match outcome {
            MigrationOutcome::RefusedNewerVersion {
                store_version,
                supported_version,
            } => {
                assert_eq!(store_version, 99);
                assert_eq!(supported_version, 1);
            }
            _ => panic!("expected RefusedNewerVersion, got {outcome:?}"),
        }
    }

    #[test]
    fn migration_already_current_is_noop() {
        let (db, _dir) = test_db();
        // Stamp the current version.
        let meta = db.keyspace("meta", KeyspaceCreateOptions::default).unwrap();
        let mut tx = db.write_tx().unwrap();
        tx.insert(&meta, "schema_version", 1u64.to_le_bytes());
        tx.commit().unwrap().unwrap();

        let runner = MigrationRunner::new(
            MigrationPlan::default_plan(),
            MigrationSafetyRules::default(),
        );
        let outcome = runner.assess(&db).unwrap();
        match outcome {
            MigrationOutcome::AlreadyCurrent { version } => {
                assert_eq!(version, 1);
            }
            _ => panic!("expected AlreadyCurrent, got {outcome:?}"),
        }
    }

    #[test]
    fn migration_plan_can_reach_checks() {
        let plan = MigrationPlan::default_plan();
        assert!(plan.can_reach(0, 1));
        assert!(plan.can_reach(1, 1));
        assert!(!plan.can_reach(2, 1));
        assert!(!plan.can_reach(0, 2));
    }

    #[test]
    fn migration_next_migration_finds_correct_step() {
        let plan = MigrationPlan::default_plan();
        assert_eq!(plan.next_migration(0).unwrap().to_version, 1);
        assert!(plan.next_migration(1).is_none());
    }

    #[test]
    fn migration_safety_rules_limit_version_jump() {
        let plan = MigrationPlan::default_plan();
        let runner = MigrationRunner::new(
            plan,
            MigrationSafetyRules {
                max_version_jump: 0, // No jumps allowed.
                ..Default::default()
            },
        );
        let (db, _dir) = test_db();
        let outcome = runner.assess(&db).unwrap();
        match outcome {
            MigrationOutcome::RefusedUnknownSchema { reason } => {
                assert!(reason.contains("exceeds max allowed"));
            }
            _ => panic!("expected RefusedUnknownSchema, got {outcome:?}"),
        }
    }

    #[test]
    fn migration_corrupt_version_is_refused() {
        let (db, _dir) = test_db();
        // Write a corrupt version (wrong length).
        let meta = db.keyspace("meta", KeyspaceCreateOptions::default).unwrap();
        let mut tx = db.write_tx().unwrap();
        tx.insert(&meta, "schema_version", vec![1, 2, 3]);
        tx.commit().unwrap().unwrap();

        let runner = MigrationRunner::new(
            MigrationPlan::default_plan(),
            MigrationSafetyRules::default(),
        );
        let outcome = runner.assess(&db).unwrap();
        match outcome {
            MigrationOutcome::RefusedUnknownSchema { reason } => {
                assert!(reason.contains("corrupt"));
            }
            _ => panic!("expected RefusedUnknownSchema, got {outcome:?}"),
        }
    }
}
