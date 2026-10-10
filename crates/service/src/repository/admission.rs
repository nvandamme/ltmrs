//! Retry-namespace lifecycle, admission and expiry collection (moved verbatim from `repository.rs`).

use fjall::{OptimisticWriteTx, Readable};

use super::{
    AdmittedScope, CanonicalRepository, DEFAULT_NAMESPACE_TTL_MILLIS, MAX_RETRIES, NamespacePin,
    decode, namespace_key, ns_key, receipt_matches_namespace,
};
use ltmrs_domain::command::{
    CommandContext, DomainError, DomainErrorCode, DomainResult, OperationScope, RetryNamespace,
};
use ltmrs_domain::id::{ChannelId, FrontendId};

impl CanonicalRepository {
    /// Resume an existing retry namespace for unknown-outcome recovery
    /// (P1-B): returns the SAME epoch without minting a new one, so a
    /// reconnected frontend keeps resolving its pre-failure receipts. The
    /// namespace must exist, belong to this (frontend, channel) pair
    /// (keyed + stored), and be within TTL — otherwise refused as stale
    /// (the caller must surface an unknown outcome, never silently mint a
    /// fresh epoch for an uncertain mutation). A sibling channel resuming
    /// the same epoch is refused: retry namespaces are channel-scoped.
    /// Read-only: no counter bump, no barrier.
    pub fn resume_namespace(
        &self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        retry_epoch: u64,
        now_millis: u64,
    ) -> DomainResult<RetryNamespace> {
        match self.lookup_namespace(frontend_id, retry_epoch)? {
            Some(ns) if ns.channel_id != channel_id => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "retry namespace belongs to another channel",
            )),
            Some(ns) if ns.is_valid_at(now_millis) => Ok(ns),
            Some(_) => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "retry namespace expired",
            )),
            None => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "unknown retry namespace",
            )),
        }
    }

    /// Issue a new retry namespace for one channel with the default TTL.
    /// Called by the daemon when a frontend authenticates. Epoch allocation
    /// reads inside the write transaction (creating a read dependency), so
    /// concurrent issuers conflict and retry instead of double-issuing the
    /// same epoch. A corrupt epoch counter fails closed (epoch reuse would
    /// confuse replays across channels).
    pub fn issue_namespace(
        &self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        now_millis: u64,
    ) -> DomainResult<RetryNamespace> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let key = namespace_key(frontend_id);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let current_epoch = match tx
                .get(&self.namespaces, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                Some(raw) => {
                    let bytes = raw.as_ref();
                    if bytes.len() == 8 {
                        let mut arr = [0u8; 8];
                        arr.copy_from_slice(bytes);
                        u64::from_le_bytes(arr).checked_add(1).ok_or_else(|| {
                            DomainError::new(
                                DomainErrorCode::Validation,
                                "namespace epoch counter exhausted",
                            )
                        })?
                    } else {
                        return Err(DomainError::new(
                            DomainErrorCode::Validation,
                            "corrupt namespace epoch counter",
                        ));
                    }
                }
                None => 1,
            };

            let ns = RetryNamespace::new(
                frontend_id,
                channel_id,
                current_epoch,
                now_millis,
                DEFAULT_NAMESPACE_TTL_MILLIS,
            );

            // Persist the namespace and its fixed expiry.
            tx.insert(&self.namespaces, &key, current_epoch.to_le_bytes());
            let ns_raw = serde_json::to_vec(&ns)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.namespaces, ns_key(&ns), &ns_raw);
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(ns);
                }
                Ok(Err(_)) => continue,
                Err(e) => return Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
            }
        }
        Err(DomainError::new(
            DomainErrorCode::Contention,
            "namespace issue conflicted (transient write contention): retry the handshake",
        ))
    }

    /// SSI-exhaustion signal (transient, safe to retry): every write
    /// path that runs out of conflict budget reports Contention, never
    /// Validation — callers and hosts must be able to tell "retry" from
    /// "refused" (see `DomainErrorCode::Contention`). Each site keeps its
    /// specific message; only the code is unified.
    pub(crate) fn exhausted_contention(message: &str) -> DomainError {
        DomainError::new(DomainErrorCode::Contention, message)
    }

    /// Advance the mutation watermark inside the caller's write
    /// transaction (atomic with the mutation itself). Keys are per-writer
    /// (`op_seq:{frontend}` for commands, `op_seq:direct` for the direct
    /// primitives) so concurrent writers never collide on one global key —
    /// a single shared counter would serialize every write under SSI and
    /// collapse concurrent throughput. Shared by the command path and the
    /// direct-write primitives (guides, suggestions, distill side-effects)
    /// so every canonical write counts — the restore delta would otherwise
    /// miss non-command writes. Operational bookkeeping (epochs, projection
    /// jobs, generation lifecycle) does not bump it: only knowledge
    /// records count.
    pub(crate) fn bump_op_seq_tx(&self, tx: &mut OptimisticWriteTx, key: &str) -> DomainResult<()> {
        let seq = match tx
            .get(&self.namespaces, key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
        {
            Some(raw) => {
                let bytes = raw.as_ref();
                if bytes.len() != 8 {
                    return Err(DomainError::new(
                        DomainErrorCode::Validation,
                        "corrupt op sequence counter",
                    ));
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(bytes);
                u64::from_le_bytes(arr)
            }
            None => 0,
        };
        let next = seq.checked_add(1).ok_or_else(|| {
            DomainError::new(DomainErrorCode::Validation, "op sequence exhausted")
        })?;
        tx.insert(&self.namespaces, key, next.to_le_bytes());
        Ok(())
    }

    /// Look up a retry namespace by frontend and epoch.
    pub fn lookup_namespace(
        &self,
        frontend_id: FrontendId,
        retry_epoch: u64,
    ) -> DomainResult<Option<RetryNamespace>> {
        let snapshot = self.db.read_tx();
        let key = format!("ns:{}:{}", frontend_id.as_uuid(), retry_epoch);
        let raw = snapshot
            .get(&self.namespaces, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode::<RetryNamespace>(v.as_ref()))
            .transpose()
    }

    /// Monotonic mutation watermark: sum over per-writer counters
    /// (`op_seq:{frontend}` + `op_seq:direct`). Restores bind
    /// preview/confirm to it so writes landing between the two are
    /// acknowledged, never drained unseen. Absent on old stores: reads as
    /// 0. A malformed counter fails closed (storage trouble is never
    /// silently skipped).
    pub fn op_seq(&self) -> DomainResult<u64> {
        let snapshot = self.db.read_tx();
        let mut total = 0u64;
        for kv in snapshot.iter(&self.namespaces) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if !String::from_utf8_lossy(k.as_ref()).starts_with("op_seq:") {
                continue;
            }
            let bytes = v.as_ref();
            if bytes.len() != 8 {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "corrupt op sequence counter",
                ));
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            total = total.checked_add(u64::from_le_bytes(arr)).ok_or_else(|| {
                DomainError::new(DomainErrorCode::Validation, "op sequence exhausted")
            })?;
        }
        Ok(total)
    }

    /// Garbage-collect expired namespaces and their receipts.
    /// Called periodically by the daemon. Returns the number of receipts removed.
    pub fn gc_expired(&self, now_millis: u64) -> DomainResult<usize> {
        let _restore_guard = self.restore_lock.read().unwrap();
        // Find expired namespaces on a read snapshot. Undecodable entries
        // are corrupt (every reader fails closed on them): collect them
        // for removal below so one bad record cannot leak forever while
        // GC keeps skipping it. A corrupt namespace also orphans its
        // receipts (matching needs the decoded value), so receipts for
        // its frontend are collected too; other frontends are untouched.
        // Malformed watermark keys heal the same way (otherwise op_seq
        // fails closed forever and bricks preview/confirm).
        let snapshot = self.db.read_tx();
        let mut expired: Vec<RetryNamespace> = Vec::new();
        let mut corrupt_keys: Vec<String> = Vec::new();
        // (frontend, epoch) scopes for orphaned receipts. The epoch comes
        // from the corrupt key itself (`ns:{fe}:{epoch}`): scoping to it
        // keeps live epochs' receipts intact (RQ-06 replay). Unparseable
        // keys heal key-only; their receipts strand until a valid same-
        // scope record expires normally (documented residual).
        let mut corrupt_scopes: Vec<(String, Option<String>)> = Vec::new();
        for kv in snapshot.iter(&self.namespaces) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let key_str = String::from_utf8_lossy(k.as_ref());
            if key_str.starts_with("ns:") {
                match decode::<RetryNamespace>(v.as_ref()) {
                    Ok(ns) if !ns.is_valid_at(now_millis) => expired.push(ns),
                    Ok(_) => {}
                    Err(_) => {
                        corrupt_keys.push(key_str.to_string());
                        // ns:{frontend}:{epoch}: scope the orphaned
                        // receipts to this exact epoch so live epochs of
                        // the same frontend keep their replay state.
                        let scope = match key_str.split(':').collect::<Vec<_>>()[..] {
                            [_, fe, epoch] => (fe.to_string(), Some(epoch.to_string())),
                            _ => (String::new(), None),
                        };
                        corrupt_scopes.push(scope);
                    }
                }
            } else if key_str.starts_with("op_seq:") && v.as_ref().len() != 8 {
                // Malformed watermark: op_seq() fails closed on it, so GC
                // heals it (accounting restarts; preview/confirm unblock).
                corrupt_keys.push(key_str.to_string());
            }
        }
        // Legacy direct receipts (pre-scope upgrade): keyed by bare
        // operation ID, unreachable by scoped keys and unattributable to
        // any namespace — sweep unconditionally so they cannot replay
        // across the upgrade or leak forever. Scoped keys carry colons;
        // bare UUIDs never do.
        let mut legacy_op_keys: Vec<String> = Vec::new();
        for ks in [&self.session_ops, &self.guide_ops, &self.suggestion_ops] {
            for kv in snapshot.iter(ks) {
                let (k, _) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let key_str = String::from_utf8_lossy(k.as_ref()).to_string();
                if !key_str.contains(':') {
                    legacy_op_keys.push(key_str);
                }
            }
        }

        if expired.is_empty() && corrupt_keys.is_empty() && legacy_op_keys.is_empty() {
            return Ok(0);
        }

        // Remove expired namespaces and their receipts in one transaction.
        // Keys are collected before removing (restore_replace precedent):
        // removing while iterating the same keyspace risks skipping
        // entries on iterators without snapshot isolation, orphaning
        // receipts no future GC re-triggers for.
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut removed = 0;
        // Keyspaces swept per expired namespace (canonical receipts plus
        // all three direct op logs — one shared key shape).
        let sweep = [
            &self.receipts,
            &self.session_ops,
            &self.guide_ops,
            &self.suggestion_ops,
            &self.tool_results,
        ];
        // Same synchronization boundary as admission: check-and-collect
        // holds this mutex across the whole collect+commit, so an
        // admission's validate-and-pin can never interleave between the
        // pinned check below and the removal. (GC is rare; brief
        // admission stalls during a collect are acceptable.)
        let pins = self.ns_pins.lock().unwrap_or_else(|e| e.into_inner());
        for ns in &expired {
            // Pinned namespaces survive expiry: a live admitted operation
            // may still finalize against them (freeze/replay/claim). They
            // collect on a later pass once unpinned. Legacy rows sweep
            // below regardless (unattributable to any live operation).
            if pins
                .get(&Self::ns_pin_key(ns.frontend_id, ns.retry_epoch))
                .is_some_and(|n| *n > 0)
            {
                continue;
            }
            // Remove every operation receipt issued under this retry
            // epoch: canonical receipts plus all three direct op logs
            // (they share the generation:frontend:epoch:operation shape).
            let mut doomed: Vec<(usize, String)> = Vec::new();
            for (i, ks) in sweep.iter().enumerate() {
                for kv in tx.iter(ks) {
                    let (k, _) = kv.into_inner().map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    let key_str = String::from_utf8_lossy(k.as_ref()).to_string();
                    if receipt_matches_namespace(&key_str, ns) {
                        doomed.push((i, key_str));
                    }
                }
            }
            for (i, key) in doomed {
                tx.remove(sweep[i], &key);
                removed += 1;
            }
            let ns_key = ns_key(ns);
            tx.remove(&self.namespaces, &ns_key);
        }
        // Legacy bare-key direct receipts: unreachable post-upgrade, swept
        // here so the keyspaces stay scope-pure.
        for ks in sweep.iter().skip(1) {
            for key in &legacy_op_keys {
                tx.remove(ks, key);
                removed += 1;
            }
        }
        // Corrupt records are undecodable everywhere (all readers fail
        // closed on them): removing heals the leak without changing any
        // observable outcome. A corrupt namespace also strands its
        // receipts (no trigger can ever match them again), so receipts
        // for its exact (frontend, epoch) scope go too — live epochs keep
        // their RQ-06 replay state; other frontends are untouched.
        // Loud: silent healing would mask storage trouble.
        if !corrupt_keys.is_empty() {
            eprintln!(
                "ltmrs: GC healing {} corrupt namespace/watermark record(s)",
                corrupt_keys.len()
            );
        }
        for key in &corrupt_keys {
            tx.remove(&self.namespaces, key);
        }
        if !corrupt_scopes.is_empty() {
            let mut doomed = Vec::new();
            for kv in tx.iter(&self.receipts) {
                let (k, _) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let key_str = String::from_utf8_lossy(k.as_ref()).to_string();
                let parts: Vec<&str> = key_str.split(':').collect();
                if parts.len() == 4
                    && corrupt_scopes.iter().any(|(fe, epoch)| {
                        parts[1] == fe && epoch.as_deref().is_none_or(|e| parts[2] == e)
                    })
                {
                    doomed.push(key_str);
                }
            }
            for key in doomed {
                tx.remove(&self.receipts, &key);
                removed += 1;
            }
        }

        match tx.commit() {
            Ok(Ok(())) => {
                self.persist_barrier()?;
                Ok(removed)
            }
            Ok(Err(_)) => Err(Self::exhausted_contention("gc conflicted")),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Validate that the command's retry namespace is still valid and
    /// belongs to the calling channel. Expired, unknown, or cross-channel
    /// namespaces are refused as stale.
    pub(crate) fn validate_namespace(&self, ctx: &CommandContext) -> DomainResult<()> {
        let now = self.clock.now_millis();
        match self.lookup_namespace(ctx.frontend_id, ctx.retry_epoch)? {
            Some(ns) if ns.channel_id != ctx.channel_id => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "retry namespace belongs to another channel",
            )),
            Some(ns) if ns.is_valid_at(now) => Ok(()),
            Some(_) => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "retry namespace expired",
            )),
            None => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "unknown retry namespace",
            )),
        }
    }

    /// Validate one tool operation's scope (RQ-06): the retry namespace
    /// must exist, belong to this channel, and be within TTL — otherwise
    /// the mutation is refused as stale instead of executing outside any
    /// namespace. Every direct guide/session/suggestion primitive calls
    /// this first, so expiry means the same thing for every MCP mutation.
    pub fn validate_scope(&self, scope: &OperationScope) -> DomainResult<()> {
        let now = self.clock.now_millis();
        match self.lookup_namespace(scope.frontend_id, scope.retry_epoch)? {
            Some(ns) if ns.channel_id != scope.channel_id => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "retry namespace belongs to another channel",
            )),
            Some(ns) if ns.is_valid_at(now) => Ok(()),
            Some(_) => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "retry namespace expired",
            )),
            None => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "unknown retry namespace",
            )),
        }
    }

    /// Admit one tool operation (RQ-06): validates its scope (existence +
    /// channel + TTL) and pins the namespace against GC expiry collection
    /// for the admitted operation's lifetime. The dispatch gate and every
    /// primary-transaction primitive admit implicitly by validating;
    /// continuation steps of the same admitted operation (response
    /// freeze, receipt replay, continuity claim, link tracking) take the
    /// returned token instead of revalidating, so a namespace expiring
    /// between primary commit and response finalization can never turn
    /// an executed operation into an error.
    pub fn admit_scope(&self, scope: &OperationScope) -> DomainResult<AdmittedScope> {
        // One synchronization boundary for the namespace lifecycle:
        // validation and pinning are atomic with respect to GC's
        // check-and-collect (which holds the same mutex), so GC can
        // never collect a namespace between this admission's validity
        // check and its pin.
        let mut pins = self.ns_pins.lock().unwrap_or_else(|e| e.into_inner());
        self.validate_scope(scope)?;
        let key = Self::ns_pin_key(scope.frontend_id, scope.retry_epoch);
        *pins.entry(key.clone()).or_insert(0) += 1;
        drop(pins);
        Ok(AdmittedScope {
            scope: scope.clone(),
            _pin: NamespacePin {
                pins: std::sync::Arc::clone(&self.ns_pins),
                key,
            },
        })
    }

    /// Pin-map key for a retry namespace (matches regardless of channel:
    /// pinning protects the whole epoch's receipts while admitted).
    fn ns_pin_key(frontend_id: FrontendId, retry_epoch: u64) -> String {
        format!("{}:{}", frontend_id.as_uuid(), retry_epoch)
    }

    /// Verify a stored direct receipt belongs to the calling scope.
    /// Defense in depth alongside the scoped keys: a receipt replays only
    /// for the generation/frontend/channel/epoch that recorded it.
    pub(crate) fn check_scope_owner(
        record_scope: Option<&OperationScope>,
        scope: &OperationScope,
    ) -> DomainResult<()> {
        match record_scope {
            Some(s)
                if s.store_generation == scope.store_generation
                    && s.frontend_id == scope.frontend_id
                    && s.channel_id == scope.channel_id
                    && s.retry_epoch == scope.retry_epoch =>
            {
                Ok(())
            }
            _ => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "receipt belongs to another scope",
            )),
        }
    }
}
