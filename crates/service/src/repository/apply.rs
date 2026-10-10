//! Centralized command application: idempotent, atomic, precondition-checked (moved verbatim from `repository.rs`).

use fjall::{OptimisticWriteTx, Readable};

use super::{
    CanonicalRepository, MAX_RETRIES, decode_receipt, encode_receipt, op_seq_key_for_scope,
    receipt_key,
};
use crate::repository_internal::{CommandState, TxAction, apply_command};
use ltmrs_domain::command::{
    CommandContext, CommandReceipt, DomainCommand, DomainError, DomainErrorCode, DomainResult,
};

impl CanonicalRepository {
    /// Centralized command application: idempotent, atomic, precondition-checked.
    pub fn apply(&self, ctx: &CommandContext, cmd: &DomainCommand) -> DomainResult<CommandReceipt> {
        let _restore_guard = self.restore_lock.read().unwrap();
        // Idempotency before admission: a committed operation replays from
        // its durable receipt even when its namespace has since expired —
        // replay creates no new effects, so a dead epoch must not hide it.
        // Owner + digest are enforced by replay_or_conflict, exactly as on
        // the transaction path below. Fresh work still validates next.
        if let Some(r) = self.lookup_receipt(
            ctx.store_generation,
            ctx.frontend_id,
            ctx.retry_epoch,
            ctx.operation_id,
        )? {
            let receipt = self.replay_or_conflict(r, ctx)?;
            self.persist_barrier()?;
            return Ok(receipt);
        }

        // Validate the retry namespace: expired or unknown namespaces are
        // refused as stale, not silently converted into new work.
        self.validate_namespace(ctx)?;

        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            match self.apply_in_tx(&mut tx, ctx, cmd)? {
                TxAction::Commit(receipt) => {
                    // Fault injection: simulate an unknown commit outcome
                    // (write may or may not have succeeded, ACK lost).
                    if self.fault_injector.inject_unknown_outcome() {
                        // Drop the transaction without committing — simulates a
                        // crash before the durability barrier. The store must
                        // remain consistent; the receipt is NOT recorded.
                        drop(tx);
                        return self.resolve_unknown_outcome(
                            ctx,
                            fjall::Error::Io(std::io::Error::other("injected unknown outcome")),
                        );
                    }
                    match tx.commit() {
                        Ok(Ok(())) => {
                            self.persist_barrier()?;
                            self.fire_commit_hook();
                            return Ok(receipt);
                        }
                        Ok(Err(_conflict)) => {
                            // Storage conflict: retry from a fresh snapshot.
                            continue;
                        }
                        Err(io_err) => {
                            // Unknown commit outcome: resolve via the receipt.
                            return self.resolve_unknown_outcome(ctx, io_err);
                        }
                    }
                }
                TxAction::Replay(receipt) => {
                    tx.rollback();
                    self.persist_barrier()?;
                    return Ok(receipt);
                }
            }
        }
        Err(Self::exhausted_contention(
            "max transaction retries exceeded",
        ))
    }

    fn apply_in_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        ctx: &CommandContext,
        cmd: &DomainCommand,
    ) -> DomainResult<TxAction<CommandReceipt>> {
        // Re-check the receipt inside the transaction to handle the race where
        // another transaction committed it after our fast-path read.
        let key = receipt_key(
            ctx.store_generation,
            ctx.frontend_id,
            ctx.retry_epoch,
            ctx.operation_id,
        );
        if let Some(raw) = tx
            .get(&self.receipts, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
        {
            let existing = decode_receipt(raw.as_ref())?;
            Self::check_replay_owner(&existing, ctx)?;
            if existing.request_digest == ctx.request_digest {
                return Ok(TxAction::Replay(existing));
            }
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }

        // Transaction-level generation fence: the command's generation must
        // equal the generation visible inside this transaction. The
        // dispatcher checks before dispatch, but only this comparison closes
        // the race where a restore cut over between dispatch and commit —
        // a fencing token must never fail open.
        let live = self.resolve_generation(tx)?;
        if live != ctx.store_generation {
            return Err(DomainError::new(
                DomainErrorCode::StaleGeneration,
                format!(
                    "stale store generation {} (live {}): re-handshake and retry",
                    ctx.store_generation.as_u64(),
                    live.as_u64(),
                ),
            ));
        }

        // Validate preconditions and apply the command inside the transaction.
        let mut state = CommandState::new(
            tx,
            &self.memories,
            &self.relations,
            &self.aliases,
            &self.projections,
            &self.generations,
            &self.feedback_events,
            self.clock.now_millis(),
        );
        let outcome = apply_command(&mut state, ctx, cmd)?;

        // Store the receipt atomically with the command.
        let receipt = CommandReceipt {
            operation_id: ctx.operation_id,
            store_generation: ctx.store_generation,
            frontend_id: ctx.frontend_id,
            channel_id: ctx.channel_id,
            request_digest: ctx.request_digest.clone(),
            outcome: outcome.clone(),
            retry_epoch: ctx.retry_epoch,
        };
        let raw = encode_receipt(&receipt)?;
        tx.insert(&self.receipts, &key, &raw);

        // Mutation watermark for restore preview/confirm binding: every
        // executed command advances it atomically with its receipt, so a
        // restore can tell whether the live store moved since the preview.
        // Replays return before this point and advance nothing. Per-frontend
        // key: concurrent frontends never collide on one global counter.
        self.bump_op_seq_tx(tx, &op_seq_key_for_scope(ctx.frontend_id, ctx.channel_id))?;

        Ok(TxAction::Commit(receipt))
    }

    fn resolve_unknown_outcome(
        &self,
        ctx: &CommandContext,
        io_err: fjall::Error,
    ) -> DomainResult<CommandReceipt> {
        // The commit result is ambiguous. Re-read the receipt with the same
        // operation key to determine whether the command actually committed.
        match self.lookup_receipt(
            ctx.store_generation,
            ctx.frontend_id,
            ctx.retry_epoch,
            ctx.operation_id,
        )? {
            Some(r) if r.request_digest == ctx.request_digest => {
                Self::check_replay_owner(&r, ctx)?;
                // The write did commit: barrier before acknowledging it.
                self.persist_barrier()?;
                Ok(r)
            }
            Some(_) => Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            )),
            None => Err(DomainError::new(
                DomainErrorCode::Validation,
                format!("commit outcome unknown and no receipt recorded: {io_err}"),
            )),
        }
    }

    fn replay_or_conflict(
        &self,
        existing: CommandReceipt,
        ctx: &CommandContext,
    ) -> DomainResult<CommandReceipt> {
        Self::check_replay_owner(&existing, ctx)?;
        if existing.request_digest == ctx.request_digest {
            Ok(existing)
        } else {
            Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ))
        }
    }

    /// Defense-in-depth receipt ownership: a receipt replays only for the
    /// channel that recorded it. The namespace gates (resume/validate)
    /// already enforce this — receipts additionally carry their channel so
    /// a cross-channel replay can never resolve, even if a namespace
    /// check is ever bypassed.
    fn check_replay_owner(existing: &CommandReceipt, ctx: &CommandContext) -> DomainResult<()> {
        if existing.frontend_id != ctx.frontend_id || existing.channel_id != ctx.channel_id {
            return Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "receipt belongs to another channel",
            ));
        }
        Ok(())
    }
}
