//! Restore coordinator: preview tokens and confirm (moved verbatim from `restore.rs`).

use std::path::PathBuf;

use super::{
    ConfirmRequest, ImportConfirmRequest, ImportCoordinator, ImportError, ImportPreview,
    ImportPreviewRequest, PreviewRecord, PreviewRequest, RESTORE_PREVIEW_TTL_MILLIS,
    RestoreCoordinator, RestoreError, RestorePreview,
};

impl RestoreCoordinator {
    pub fn preview(&mut self, req: PreviewRequest<'_>) -> RestorePreview {
        let backup = req.backup;
        let live_counts = req.live_counts;
        let live_generation = req.live_generation;
        let active_channels = req.active_channels;
        let now_millis = req.now_millis;
        self.evict_expired(now_millis);
        if active_channels > 1 {
            return RestorePreview {
                ready: false,
                message: format!(
                    "{} other connections are open; close them and preview again (restore replaces, never merges)",
                    active_channels - 1
                ),
                unknown_top_level: backup.unknown_top_level,
                confirmation_token: None,
                expires_at: None,
            };
        }
        let token = uuid::Uuid::now_v7().to_string();
        let live_total: u64 = live_counts.values().sum();
        let backup_total: u64 = backup.counts.values().sum();
        self.pending.insert(
            token.clone(),
            PreviewRecord {
                digest: backup.digest.clone(),
                generation: live_generation,
                created_at: now_millis,
                used: false,
                source: req.source_path.to_path_buf(),
                channel: req.channel.to_string(),
                op_seq: req.live_op_seq,
            },
        );
        RestorePreview {
            ready: true,
            message: format!(
                "Backup holds {backup_total} records (digest {}) vs live {live_total}; confirm replaces the live store (never merges).{}",
                &backup.digest[..backup.digest.len().min(12)],
                if backup.unknown_top_level == 0 {
                    String::new()
                } else {
                    format!(
                        " {} unknown top-level key(s) will be dropped on restore (counted, not restored).",
                        backup.unknown_top_level
                    )
                }
            ),
            unknown_top_level: backup.unknown_top_level,
            confirmation_token: Some(token),
            expires_at: Some(now_millis + RESTORE_PREVIEW_TTL_MILLIS),
        }
    }

    /// Peek the source path a token was previewed from (no consumption;
    /// lets the caller re-verify the file before confirming).
    pub fn source_path(&self, token: &str) -> Option<PathBuf> {
        self.pending.get(token).map(|rec| rec.source.clone())
    }

    /// Drop expired or consumed records (idempotent housekeeping).
    fn evict_expired(&mut self, now_millis: u64) {
        self.pending.retain(|_, rec| {
            !rec.used && now_millis <= rec.created_at + RESTORE_PREVIEW_TTL_MILLIS
        });
    }

    /// Confirm a previewed restore. Returns the bound digest (so the caller
    /// re-verifies the file before replacing anything) plus the live writes
    /// that landed between preview and confirm: the replace drains them, so
    /// the caller must acknowledge the count (recoverable from the safety
    /// backup) instead of dropping it silently. Refusing here would brick
    /// restores for active hosts (their own channel writes mid-flow), so
    /// the delta is reported, not refused. `active_channels` counts
    /// cooperating connections including the caller: more than one
    /// re-checks the preview lease (a connection that arrived after the
    /// preview may hold acknowledged writes the replace would drain unseen).
    /// Lease failures do NOT consume the token: close the others and
    /// confirm again.
    pub fn confirm(&mut self, req: ConfirmRequest<'_>) -> Result<(String, u64), RestoreError> {
        let token = req.token;
        let rec = self
            .pending
            .get(token)
            .cloned()
            .ok_or(RestoreError::InvalidToken)?;
        if req.now_millis > rec.created_at + RESTORE_PREVIEW_TTL_MILLIS {
            self.pending.remove(token);
            return Err(RestoreError::Expired);
        }
        if rec.used {
            return Err(RestoreError::AlreadyUsed);
        }
        if !req.confirm {
            return Err(RestoreError::NeedsConfirm);
        }
        if req.digest != rec.digest {
            // The file changed under us: the preview is meaningless now.
            self.pending.remove(token);
            return Err(RestoreError::StaleSource {
                expected: rec.digest,
                actual: req.digest.to_string(),
            });
        }
        if req.channel != rec.channel {
            self.pending.remove(token);
            return Err(RestoreError::Blocked(format!(
                "confirmation arrived on a different channel (previewed on {})",
                rec.channel
            )));
        }
        if req.live_generation != rec.generation {
            self.pending.remove(token);
            return Err(RestoreError::GenerationChanged {
                expected: rec.generation,
                actual: req.live_generation,
            });
        }
        if req.active_channels > 1 {
            return Err(RestoreError::Blocked(format!(
                "{} other connections are open; close them and confirm again (the token stays valid)",
                req.active_channels - 1
            )));
        }
        if let Some(stored) = self.pending.get_mut(token) {
            stored.used = true;
        }
        Ok((rec.digest, req.live_op_seq.saturating_sub(rec.op_seq)))
    }
}

impl ImportCoordinator {
    /// Preview a verified staging against the live store. More than one
    /// cooperating connection blocks (same rule as restore: the replace
    /// drains acknowledged writes).
    pub fn preview(&mut self, req: ImportPreviewRequest<'_>) -> ImportPreview {
        self.evict_expired(req.now_millis);
        let key = (req.channel.to_string(), req.source_path.to_path_buf());
        if req.active_channels > 1 {
            return ImportPreview {
                ready: false,
                message: format!(
                    "{} other connections are open; close them and preview again (import replaces, never merges)",
                    req.active_channels - 1
                ),
                unknown_top_level: req.backup.unknown_top_level,
            };
        }
        let live_total: u64 = req.live_counts.values().sum();
        let backup_total: u64 = req.backup.counts.values().sum();
        self.pending.insert(
            key,
            super::LeaseRecord {
                digest: req.backup.digest.clone(),
                generation: req.live_generation,
                created_at: req.now_millis,
                used: false,
                live_op_seq: req.live_op_seq,
            },
        );
        ImportPreview {
            ready: true,
            message: format!(
                "Import holds {backup_total} records (digest {}) vs live {live_total}; confirm replaces the live store (never merges).{}",
                &req.backup.digest[..req.backup.digest.len().min(12)],
                if req.backup.unknown_top_level == 0 {
                    String::new()
                } else {
                    format!(
                        " {} loss item(s) counted in staging (reported, not restored silently).",
                        req.backup.unknown_top_level
                    )
                }
            ),
            unknown_top_level: req.backup.unknown_top_level,
        }
    }

    /// Drop expired or consumed leases (idempotent housekeeping).
    fn evict_expired(&mut self, now_millis: u64) {
        self.pending.retain(|_, rec| {
            !rec.used && now_millis <= rec.created_at + RESTORE_PREVIEW_TTL_MILLIS
        });
    }

    /// Confirm a previewed import. Returns the bound digest plus the live
    /// writes that landed between preview and confirm (acknowledged by the
    /// caller, recoverable from the safety backup). Lease failures do NOT
    /// consume the lease except stale/expired ones — except `AlreadyUsed`.
    pub fn confirm(&mut self, req: ImportConfirmRequest<'_>) -> Result<(String, u64), ImportError> {
        let key = (req.channel.to_string(), req.source_path.to_path_buf());
        let rec = self
            .pending
            .get(&key)
            .cloned()
            .ok_or(ImportError::NeedsPreview)?;
        if req.now_millis > rec.created_at + RESTORE_PREVIEW_TTL_MILLIS {
            self.pending.remove(&key);
            return Err(ImportError::Expired);
        }
        if rec.used {
            return Err(ImportError::AlreadyUsed);
        }
        if req.digest != rec.digest {
            self.pending.remove(&key);
            return Err(ImportError::StaleSource {
                expected: rec.digest,
                actual: req.digest.to_string(),
            });
        }
        if req.live_generation != rec.generation {
            self.pending.remove(&key);
            return Err(ImportError::GenerationChanged {
                expected: rec.generation,
                actual: req.live_generation,
            });
        }
        if req.active_channels > 1 {
            return Err(ImportError::Blocked(format!(
                "{} other connections are open; close them and confirm again (the lease stays valid)",
                req.active_channels - 1
            )));
        }
        if let Some(stored) = self.pending.get_mut(&key) {
            stored.used = true;
        }
        Ok((rec.digest, req.live_op_seq.saturating_sub(rec.live_op_seq)))
    }
}
