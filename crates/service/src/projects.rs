//! Stable project registry + identity resolution (Wave A3b, Part I 01:§6.3).
//!
//! The daemon owns the `projects` keyspace (`id → record`) plus the
//! secondary `root → id` index. The frontend only resolves its bind file,
//! environment and cwd and forwards them in the envelope; the daemon never
//! uses its own cwd. Unknown ids persist on first acknowledged write,
//! known ids learn moved roots, and conflicting claims refuse with
//! candidates plus the `project bind` remediation.

use fjall::Readable;

use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::EntityId;

use crate::repository::CanonicalRepository;
use crate::repository_internal::{decode, encode};

impl CanonicalRepository {
    /// Look up a project record by its ULID identity.
    pub fn project_record(&self, id: EntityId) -> DomainResult<Option<ProjectRecord>> {
        let snapshot = self.db_ref().read_tx();
        let raw = snapshot
            .get(self.projects_ks(), project_key(id))
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode::<ProjectRecord>(v.as_ref())).transpose()
    }

    /// The identity bound to a canonical root, if any.
    pub fn project_by_root(&self, root: &str) -> DomainResult<Option<EntityId>> {
        let snapshot = self.db_ref().read_tx();
        let raw = snapshot
            .get(self.project_roots_ks(), root_key(root))
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| {
            let s = String::from_utf8_lossy(v.as_ref()).to_string();
            uuid::Uuid::parse_str(&s).map(EntityId::new).map_err(|_| {
                DomainError::new(DomainErrorCode::Validation, "corrupt project root binding")
            })
        })
        .transpose()
    }

    /// Every registered identity carrying this display name.
    pub fn projects_with_name(&self, name: &str) -> DomainResult<Vec<ProjectRecord>> {
        let snapshot = self.db_ref().read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(self.projects_ks()) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let rec: ProjectRecord = decode(v.as_ref())?;
            if rec.name == name {
                out.push(rec);
            }
        }
        out.sort_by_key(|r| r.first_seen);
        Ok(out)
    }

    /// Full `prj_…` ids of every identity that attached a legacy slug.
    pub fn legacy_bucket_names(&self, slug: &str) -> DomainResult<Vec<String>> {
        let snapshot = self.db_ref().read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(self.projects_ks()) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let rec: ProjectRecord = decode(v.as_ref())?;
            if rec.legacy_slugs.iter().any(|s| s == slug) {
                out.push(ltmrs_domain::project_id::format_project_id(
                    &rec.name, rec.id,
                ));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Legacy rows (`project == slug`, no stable id) waiting for adoption.
    pub fn count_unattached_legacy(&self, slug: &str) -> DomainResult<usize> {
        let snapshot = self.db_ref().read_tx();
        let mut n = 0usize;
        for kv in snapshot.iter(self.memories_ks()) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let m: ltmrs_domain::memory::Memory = decode(v.as_ref())?;
            if m.project_id.is_none() && m.project.as_deref() == Some(slug) {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Explicit cross-name legacy attachment (`project adopt`): records
    /// `slug` in the identity's `legacy_slugs` so its rows become visible
    /// under the stable scope. Rows are never rewritten. A slug attached
    /// elsewhere warns (shared visibility) instead of refusing — adopt is
    /// explicit intent, unlike unqualified ambiguous reads.
    pub fn adopt_legacy_slug(
        &self,
        id: EntityId,
        name: &str,
        root: Option<&str>,
        slug: &str,
    ) -> DomainResult<AdoptOutcome> {
        if slug.trim().is_empty() {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "cannot adopt an empty legacy slug: pass --slug <slug>",
            ));
        }
        let now = self.clock_now();
        let _restore_guard = self.restore_read_guard();
        for _attempt in 0..crate::repository::MAX_RETRIES {
            let mut tx = self
                .db_ref()
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let rec: Option<ProjectRecord> = match tx
                .get(self.projects_ks(), project_key(id))
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                Some(raw) => Some(decode(raw.as_ref())?),
                None => None,
            };
            match rec {
                Some(mut rec) => {
                    if rec.name != name {
                        return Err(DomainError::new(
                            DomainErrorCode::Validation,
                            format!(
                                "project id {} is registered as '{}', not '{name}'",
                                ltmrs_domain::project_id::format_project_id(name, id),
                                rec.name
                            ),
                        ));
                    }
                    if rec.legacy_slugs.iter().any(|s| s == slug) {
                        return Ok(AdoptOutcome {
                            record: rec,
                            attached: 0,
                            already_attached: true,
                            created: false,
                            warning: None,
                        });
                    }
                    let attached = self.count_unattached_legacy_tx(&tx, slug)?;
                    let others = self.attached_elsewhere_tx(&tx, slug, id)?;
                    if attached == 0 && others.is_empty() {
                        return Err(self.unknown_slug_error(&tx, slug)?);
                    }
                    rec.legacy_slugs.push(slug.to_string());
                    rec.last_seen = now;
                    let raw = encode(&rec)?;
                    tx.insert(self.projects_ks(), project_key(id), &raw);
                    match tx.commit() {
                        Ok(Ok(())) => {
                            self.durability_barrier()?;
                            return Ok(AdoptOutcome {
                                record: rec,
                                attached,
                                already_attached: false,
                                created: false,
                                warning: shared_warning(slug, &others),
                            });
                        }
                        Ok(Err(_)) => continue,
                        Err(e) => {
                            return Err(DomainError::new(
                                DomainErrorCode::Validation,
                                e.to_string(),
                            ));
                        }
                    }
                }
                None => {
                    let attached = self.count_unattached_legacy_tx(&tx, slug)?;
                    let others = self.attached_elsewhere_tx(&tx, slug, id)?;
                    if attached == 0 && others.is_empty() {
                        return Err(self.unknown_slug_error(&tx, slug)?);
                    }
                    if let Some(r) = root
                        && let Some(owner_s) = tx
                            .get(self.project_roots_ks(), root_key(r))
                            .map_err(|e| {
                                DomainError::new(DomainErrorCode::Validation, e.to_string())
                            })?
                            .map(|v| String::from_utf8_lossy(v.as_ref()).into_owned())
                    {
                        let owner = uuid::Uuid::parse_str(&owner_s).map(EntityId::new);
                        if !matches!(owner, Ok(oid) if oid == id) {
                            let owner_full = match owner {
                                Ok(oid) => match tx
                                    .get(self.projects_ks(), project_key(oid))
                                    .map_err(|e| {
                                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                                    })? {
                                    Some(raw) => {
                                        let rec: ProjectRecord = decode(raw.as_ref())?;
                                        ltmrs_domain::project_id::format_project_id(&rec.name, oid)
                                    }
                                    None => owner_s,
                                },
                                Err(_) => owner_s,
                            };
                            return Err(DomainError::new(
                                DomainErrorCode::Validation,
                                format!(
                                    "checkout root '{r}' is already bound to {owner_full}; refusing stranger claim {}; {BIND_REMEDIATION}",
                                    ltmrs_domain::project_id::format_project_id(name, id),
                                ),
                            ));
                        }
                    }
                    let rec = ProjectRecord {
                        id,
                        name: name.to_string(),
                        canonical_roots: root.into_iter().map(|s| s.to_string()).collect(),
                        first_seen: now,
                        last_seen: now,
                        legacy_slugs: vec![slug.to_string()],
                    };
                    let raw = encode(&rec)?;
                    tx.insert(self.projects_ks(), project_key(id), &raw);
                    if let Some(r) = root {
                        tx.insert(
                            self.project_roots_ks(),
                            root_key(r),
                            id.as_uuid().to_string().as_bytes(),
                        );
                    }
                    match tx.commit() {
                        Ok(Ok(())) => {
                            self.durability_barrier()?;
                            return Ok(AdoptOutcome {
                                record: rec,
                                attached,
                                already_attached: false,
                                created: true,
                                warning: shared_warning(slug, &others),
                            });
                        }
                        Ok(Err(_)) => continue,
                        Err(e) => {
                            return Err(DomainError::new(
                                DomainErrorCode::Validation,
                                e.to_string(),
                            ));
                        }
                    }
                }
            }
        }
        Err(DomainError::new(
            DomainErrorCode::Contention,
            "project adopt conflicted (transient write contention): retry the call",
        ))
    }

    /// Full ids of every OTHER identity already attaching `slug`.
    fn attached_elsewhere_tx(
        &self,
        tx: &fjall::OptimisticWriteTx,
        slug: &str,
        id: EntityId,
    ) -> DomainResult<Vec<String>> {
        let mut out = Vec::new();
        for kv in tx.iter(self.projects_ks()) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let rec: ProjectRecord = decode(v.as_ref())?;
            if rec.id != id && rec.legacy_slugs.iter().any(|s| s == slug) {
                out.push(ltmrs_domain::project_id::format_project_id(
                    &rec.name, rec.id,
                ));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Unknown-slug refusal naming the slug plus every known bucket.
    fn unknown_slug_error(
        &self,
        tx: &fjall::OptimisticWriteTx,
        slug: &str,
    ) -> DomainResult<DomainError> {
        let mut known: Vec<String> = Vec::new();
        for kv in tx.iter(self.memories_ks()) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let m: ltmrs_domain::memory::Memory = decode(v.as_ref())?;
            if m.project_id.is_none()
                && let Some(p) = m.project.as_deref()
                && !known.iter().any(|s| s == p)
            {
                known.push(p.to_string());
            }
        }
        for kv in tx.iter(self.projects_ks()) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let rec: ProjectRecord = decode(v.as_ref())?;
            for s in &rec.legacy_slugs {
                if !known.iter().any(|k| k == s) {
                    known.push(s.clone());
                }
            }
        }
        known.sort();
        let msg = if known.is_empty() {
            format!("unknown legacy slug '{slug}': no legacy buckets exist")
        } else {
            format!("unknown legacy slug '{slug}' (known: {})", known.join(", "))
        };
        Ok(DomainError::new(DomainErrorCode::Validation, msg))
    }

    /// Write-time claim: insert-if-absent, moved-root append, or refusal.
    /// Unknown ids persist on first acknowledged write (with same-name
    /// legacy adoption); known ids learn a moved root only when every
    /// recorded root is gone (otherwise the live owner keeps it).
    pub fn claim_project_for_write(
        &self,
        id: EntityId,
        name: &str,
        root: Option<&str>,
        legacy: Option<&str>,
        now: u64,
    ) -> DomainResult<ClaimOutcome> {
        let _restore_guard = self.restore_read_guard();
        for _attempt in 0..crate::repository::MAX_RETRIES {
            let mut tx = self
                .db_ref()
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let rec: Option<ProjectRecord> = match tx
                .get(self.projects_ks(), project_key(id))
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                Some(raw) => Some(decode(raw.as_ref())?),
                None => None,
            };
            match rec {
                Some(mut rec) => {
                    if rec.name != name {
                        return Err(DomainError::new(
                            DomainErrorCode::Validation,
                            format!(
                                "project id {} is registered as '{0}', not '{name}' — refusing a renamed claim",
                                ltmrs_domain::project_id::format_project_id(&rec.name, id),
                            ),
                        ));
                    }
                    if let Some(r) = root {
                        if !rec.canonical_roots.iter().any(|x| x == r) {
                            if rec.canonical_roots.iter().any(|x| path_exists(x)) {
                                let owner =
                                    ltmrs_domain::project_id::format_project_id(&rec.name, id);
                                return Err(DomainError::new(
                                    DomainErrorCode::Validation,
                                    format!(
                                        "checkout root '{r}' is not registered to {owner} (its live checkout still exists) — refusing a conflicting claim; {BIND_REMEDIATION}"
                                    ),
                                ));
                            }
                            rec.canonical_roots.push(r.to_string());
                            rec.last_seen = now;
                            let raw = encode(&rec)?;
                            tx.insert(self.projects_ks(), project_key(id), &raw);
                            tx.insert(
                                self.project_roots_ks(),
                                root_key(r),
                                id.as_uuid().to_string().as_bytes(),
                            );
                            match tx.commit() {
                                Ok(Ok(())) => {
                                    self.durability_barrier()?;
                                    return Ok(ClaimOutcome {
                                        record: rec,
                                        created: false,
                                        moved: true,
                                        adopted: 0,
                                        collision_warning: None,
                                    });
                                }
                                Ok(Err(_)) => continue,
                                Err(e) => {
                                    return Err(DomainError::new(
                                        DomainErrorCode::Validation,
                                        e.to_string(),
                                    ));
                                }
                            }
                        }
                        rec.last_seen = now;
                        let raw = encode(&rec)?;
                        tx.insert(self.projects_ks(), project_key(id), &raw);
                    }
                    match tx.commit() {
                        Ok(Ok(())) => {
                            self.durability_barrier()?;
                            return Ok(ClaimOutcome {
                                record: rec,
                                created: false,
                                moved: false,
                                adopted: 0,
                                collision_warning: None,
                            });
                        }
                        Ok(Err(_)) => continue,
                        Err(e) => {
                            return Err(DomainError::new(
                                DomainErrorCode::Validation,
                                e.to_string(),
                            ));
                        }
                    }
                }
                None => {
                    // I2: the wire string must agree with the claimed name
                    // (absent defaults to it). A foreign wire under a fresh
                    // stable id would dual-write ("evil", shop-id) with no
                    // bucket to attach to — refuse, never smuggle.
                    if let Some(wire) = legacy
                        && wire != name
                    {
                        return Err(DomainError::new(
                            DomainErrorCode::Validation,
                            format!(
                                "wire project '{wire}' does not match the claimed identity name '{name}' — pass the bound name, an attached slug, or no project"
                            ),
                        ));
                    }
                    let owner_s: Option<String> = match root {
                        Some(r) => tx
                            .get(self.project_roots_ks(), root_key(r))
                            .map_err(|e| {
                                DomainError::new(DomainErrorCode::Validation, e.to_string())
                            })?
                            .map(|v| String::from_utf8_lossy(v.as_ref()).into_owned()),
                        None => None,
                    };
                    if let Some(owner_s) = owner_s {
                        let owner_id = uuid::Uuid::parse_str(&owner_s).map(EntityId::new);
                        let owner_full = match owner_id {
                            Ok(oid) => {
                                match tx.get(self.projects_ks(), project_key(oid)).map_err(|e| {
                                    DomainError::new(DomainErrorCode::Validation, e.to_string())
                                })? {
                                    Some(raw) => {
                                        let rec: ProjectRecord = decode(raw.as_ref())?;
                                        ltmrs_domain::project_id::format_project_id(&rec.name, oid)
                                    }
                                    None => owner_s,
                                }
                            }
                            Err(_) => owner_s,
                        };
                        let claimed = ltmrs_domain::project_id::format_project_id(name, id);
                        return Err(DomainError::new(
                            DomainErrorCode::Validation,
                            format!(
                                "checkout root '{}' is already bound to {owner_full}; refusing stranger claim {claimed}; {BIND_REMEDIATION}",
                                root.unwrap_or("(unknown)")
                            ),
                        ));
                    }
                    let attach = legacy == Some(name);
                    let others_names: Vec<String> = {
                        let mut names = Vec::new();
                        for kv in tx.iter(self.projects_ks()) {
                            let (_k, v) = kv.into_inner().map_err(|e| {
                                DomainError::new(DomainErrorCode::Validation, e.to_string())
                            })?;
                            let rec: ProjectRecord = decode(v.as_ref())?;
                            if rec.name == name {
                                names.push(ltmrs_domain::project_id::format_project_id(
                                    &rec.name, rec.id,
                                ));
                            }
                        }
                        names
                    };
                    let adopted = if attach {
                        self.count_unattached_legacy_tx(&tx, name)?
                    } else {
                        0
                    };
                    let rec = ProjectRecord {
                        id,
                        name: name.to_string(),
                        canonical_roots: root.into_iter().map(|s| s.to_string()).collect(),
                        first_seen: now,
                        last_seen: now,
                        legacy_slugs: if attach {
                            vec![name.to_string()]
                        } else {
                            vec![]
                        },
                    };
                    let raw = encode(&rec)?;
                    tx.insert(self.projects_ks(), project_key(id), &raw);
                    if let Some(r) = root {
                        tx.insert(
                            self.project_roots_ks(),
                            root_key(r),
                            id.as_uuid().to_string().as_bytes(),
                        );
                    }
                    match tx.commit() {
                        Ok(Ok(())) => {
                            self.durability_barrier()?;
                            let collision_warning = if others_names.is_empty() {
                                None
                            } else {
                                Some(format!(
                                    "name '{name}' is also registered as {}; unqualified reads stay ambiguous until qualified",
                                    others_names.join(", ")
                                ))
                            };
                            return Ok(ClaimOutcome {
                                record: rec,
                                created: true,
                                moved: false,
                                adopted,
                                collision_warning,
                            });
                        }
                        Ok(Err(_)) => continue,
                        Err(e) => {
                            return Err(DomainError::new(
                                DomainErrorCode::Validation,
                                e.to_string(),
                            ));
                        }
                    }
                }
            }
        }
        Err(DomainError::new(
            DomainErrorCode::Contention,
            "project claim conflicted (transient write contention): retry the call",
        ))
    }

    /// Count unattached legacy rows inside the caller's write transaction.
    fn count_unattached_legacy_tx(
        &self,
        tx: &fjall::OptimisticWriteTx,
        slug: &str,
    ) -> DomainResult<usize> {
        let mut n = 0usize;
        for kv in tx.iter(self.memories_ks()) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let m: ltmrs_domain::memory::Memory = decode(v.as_ref())?;
            if m.project_id.is_none() && m.project.as_deref() == Some(slug) {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Resolve one call's identity (shadow matrix a–d). Reads never mutate;
    /// writes may register, append a moved root, or refuse.
    pub fn resolve_identity(
        &self,
        claim: &IdentityClaim<'_>,
        is_write: bool,
    ) -> DomainResult<ResolvedScope> {
        let now = self.clock_now();
        if let Some(full) = claim.project_id {
            return self.resolve_claimed(full, claim, is_write, now);
        }
        self.resolve_unclaimed(claim, is_write)
    }

    /// Claimed-id path: known ids resolve (roots checked), unknown ids
    /// persist on write and refuse on read.
    fn resolve_claimed(
        &self,
        full: &str,
        claim: &IdentityClaim<'_>,
        is_write: bool,
        now: u64,
    ) -> DomainResult<ResolvedScope> {
        let (name, id) = ltmrs_domain::project_id::parse_project_id(full).ok_or_else(|| {
            DomainError::new(
                DomainErrorCode::Validation,
                format!("malformed project_id '{full}' (want prj_<name>_<26-char ULID>); {BIND_REMEDIATION}"),
            )
        })?;
        match self.project_record(id)? {
            Some(rec) => {
                if rec.name != name {
                    return Err(DomainError::new(
                        DomainErrorCode::Validation,
                        format!(
                            "project id {full} is registered as '{}', not '{name}'",
                            rec.name
                        ),
                    ));
                }
                if let Some(wire) = claim.legacy_project {
                    self.check_wire_against_claim(wire, &rec)?;
                }
                match claim.canonical_root {
                    Some(r) => match self.project_by_root(r)? {
                        Some(bound) if bound == id => {
                            if is_write {
                                self.touch_last_seen(id, now)?;
                            }
                            Ok(self.stable_scope(&rec, full, is_write, None))
                        }
                        Some(_) => {
                            let owner_full = self.owner_full_for_root(r)?;
                            if is_write {
                                let claimed =
                                    ltmrs_domain::project_id::format_project_id(&rec.name, id);
                                return Err(DomainError::new(
                                    DomainErrorCode::Validation,
                                    format!(
                                        "checkout root '{r}' is registered to {owner_full} but the call claims {claimed}; {BIND_REMEDIATION}"
                                    ),
                                ));
                            }
                            let mut out = self.stable_scope(&rec, full, false, None);
                            out.warning = Some(format!(
                                "checkout root '{r}' is registered to {owner_full}, proceeding under the explicitly claimed identity"
                            ));
                            Ok(out)
                        }
                        None => {
                            if is_write {
                                let outcome = self.claim_project_for_write(
                                    id,
                                    &name,
                                    Some(r),
                                    claim.legacy_project,
                                    now,
                                )?;
                                let mut out = self.stable_scope(
                                    &outcome.record,
                                    full,
                                    true,
                                    outcome.collision_warning,
                                );
                                out.adopted = outcome.adopted;
                                out.moved = outcome.moved;
                                if outcome.moved && out.warning.is_none() {
                                    out.warning = Some(format!(
                                        "checkout moved: root '{r}' appended to {}; old roots are gone",
                                        ltmrs_domain::project_id::format_project_id(
                                            &outcome.record.name,
                                            id
                                        )
                                    ));
                                }
                                if outcome.adopted > 0 {
                                    let note = format!(
                                        "attached {} legacy '{}' fragment(s) to {}",
                                        outcome.adopted,
                                        outcome.record.name,
                                        ltmrs_domain::project_id::format_project_id(
                                            &outcome.record.name,
                                            id
                                        )
                                    );
                                    out.warning = Some(match out.warning.take() {
                                        Some(w) => format!("{w}; {note}"),
                                        None => note,
                                    });
                                }
                                Ok(out)
                            } else {
                                Ok(self.stable_scope(&rec, full, false, None))
                            }
                        }
                    },
                    None => Ok(self.stable_scope(&rec, full, is_write, None)),
                }
            }
            None => {
                if !is_write {
                    let mut candidates = self.same_name_full_ids(&name)?;
                    candidates.sort();
                    let mut msg = format!("unknown project id {full}");
                    if candidates.is_empty() {
                        msg.push_str("; it is not registered — writes register it on first use");
                    } else {
                        msg.push_str(&format!(
                            "; same-name identities exist: {}",
                            candidates.join(", ")
                        ));
                    }
                    msg.push_str(&format!("; {BIND_REMEDIATION}"));
                    return Err(DomainError::new(DomainErrorCode::Validation, msg));
                }
                let outcome = self.claim_project_for_write(
                    id,
                    &name,
                    claim.canonical_root,
                    claim.legacy_project,
                    now,
                )?;
                let full_owned =
                    ltmrs_domain::project_id::format_project_id(&outcome.record.name, id);
                let mut out = self.stable_scope(
                    &outcome.record,
                    &full_owned,
                    true,
                    outcome.collision_warning,
                );
                out.adopted = outcome.adopted;
                out.moved = outcome.moved;
                if outcome.adopted > 0 {
                    let note = format!(
                        "attached {} legacy '{}' fragment(s) to {full_owned}",
                        outcome.adopted, outcome.record.name
                    );
                    out.warning = Some(match out.warning.take() {
                        Some(w) => format!("{w}; {note}"),
                        None => note,
                    });
                }
                Ok(out)
            }
        }
    }

    /// Unclaimed path: legacy wire string or the cwd clue, with the shadow
    /// matrix for ambiguous names.
    fn resolve_unclaimed(
        &self,
        claim: &IdentityClaim<'_>,
        is_write: bool,
    ) -> DomainResult<ResolvedScope> {
        let explicit = claim.legacy_explicit;
        let base = claim.canonical_root.and_then(basename_slug);
        let eff: Option<String> = if explicit {
            claim.legacy_project.map(|s| s.to_string())
        } else {
            claim.legacy_project.map(|s| s.to_string()).or(base.clone())
        };
        let Some(slug) = eff else {
            return Ok(ResolvedScope {
                project_id: None,
                project_id_str: None,
                name: None,
                legacy: None,
                all: claim.all,
                attached: vec![],
                warning: None,
                adopted: 0,
                moved: false,
            });
        };
        let buckets = self.legacy_bucket_names(&slug)?;
        let same_name = self.projects_with_name(&slug)?;
        let ambiguous = buckets.len() >= 2
            || same_name.len() >= 2
            || (same_name.len() == 1
                && claim
                    .canonical_root
                    .is_some_and(|r| !same_name[0].canonical_roots.iter().any(|x| x == r)));
        // A checkout already bound to this name writes only under its
        // identity: a provisional write would fragment the stable set.
        let bound_here = same_name.iter().any(|rec| {
            claim
                .canonical_root
                .is_some_and(|r| rec.canonical_roots.iter().any(|x| x == r))
        });
        if ambiguous || (bound_here && is_write) {
            let mut candidates: Vec<String> = same_name
                .iter()
                .map(|rec| ltmrs_domain::project_id::format_project_id(&rec.name, rec.id))
                .collect();
            candidates.extend(buckets.iter().cloned());
            candidates.sort();
            candidates.dedup();
            if is_write {
                let mut msg = if candidates.is_empty() {
                    format!("ambiguous project '{slug}' for this checkout")
                } else {
                    format!(
                        "ambiguous project '{slug}' (candidates: {})",
                        candidates.join(", ")
                    )
                };
                msg.push_str(&format!("; {BIND_REMEDIATION}"));
                if bound_here {
                    msg.push_str(" — this checkout is already bound; restore its bind file instead of writing provisionally");
                }
                return Err(DomainError::new(DomainErrorCode::Validation, msg));
            }
            if !claim.all {
                let mut msg = if candidates.is_empty() {
                    format!("ambiguous project '{slug}' for this checkout")
                } else {
                    format!(
                        "ambiguous project '{slug}' (candidates: {})",
                        candidates.join(", ")
                    )
                };
                msg.push_str(&format!(
                    "; qualify with an explicit project id or pass all=true; {BIND_REMEDIATION}"
                ));
                return Err(DomainError::new(DomainErrorCode::Validation, msg));
            }
            return Ok(ResolvedScope {
                project_id: None,
                project_id_str: None,
                name: None,
                legacy: Some(slug.clone()),
                all: true,
                attached: vec![],
                warning: Some(if candidates.is_empty() {
                    format!("ambiguous project '{slug}': showing all projects")
                } else {
                    format!(
                        "ambiguous project '{slug}' (candidates: {}): showing all projects",
                        candidates.join(", ")
                    )
                }),
                adopted: 0,
                moved: false,
            });
        }
        Ok(ResolvedScope {
            project_id: None,
            project_id_str: None,
            name: None,
            legacy: Some(slug),
            all: claim.all,
            attached: vec![],
            warning: None,
            adopted: 0,
            moved: false,
        })
    }

    /// The wire legacy string must agree with the claimed identity (its
    /// name or an attached slug), never smuggle a foreign bucket in.
    fn check_wire_against_claim(&self, wire: &str, rec: &ProjectRecord) -> DomainResult<()> {
        if wire == rec.name || rec.legacy_slugs.iter().any(|s| s == wire) {
            return Ok(());
        }
        Err(DomainError::new(
            DomainErrorCode::Validation,
            format!(
                "wire project '{wire}' does not match the claimed identity {} — pass one scope, not two",
                ltmrs_domain::project_id::format_project_id(&rec.name, rec.id)
            ),
        ))
    }

    /// The owner line for a root conflict: the bound identity, if known.
    fn owner_full_for_root(&self, root: &str) -> DomainResult<String> {
        match self.project_by_root(root)? {
            Some(id) => match self.project_record(id)? {
                Some(rec) => Ok(ltmrs_domain::project_id::format_project_id(&rec.name, id)),
                None => Ok(id.as_uuid().to_string()),
            },
            None => Ok("(unbound)".to_string()),
        }
    }

    /// Same-name full ids, for unknown-identity refusal candidates.
    fn same_name_full_ids(&self, name: &str) -> DomainResult<Vec<String>> {
        Ok(self
            .projects_with_name(name)?
            .iter()
            .map(|rec| ltmrs_domain::project_id::format_project_id(&rec.name, rec.id))
            .collect())
    }

    /// Touch last_seen on the bound path (writes only; reads never mutate).
    fn touch_last_seen(&self, id: EntityId, now: u64) -> DomainResult<()> {
        let _restore_guard = self.restore_read_guard();
        for _attempt in 0..crate::repository::MAX_RETRIES {
            let mut tx = self
                .db_ref()
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let raw = tx
                .get(self.projects_ks(), project_key(id))
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Ok(());
            };
            let mut rec: ProjectRecord = decode(raw.as_ref())?;
            rec.last_seen = now;
            let out = encode(&rec)?;
            tx.insert(self.projects_ks(), project_key(id), &out);
            match tx.commit() {
                Ok(Ok(())) => {
                    self.durability_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Ok(())
    }

    /// The stable scope for a resolved record.
    fn stable_scope(
        &self,
        rec: &ProjectRecord,
        full: &str,
        _is_write: bool,
        warning: Option<String>,
    ) -> ResolvedScope {
        ResolvedScope {
            project_id: Some(rec.id),
            project_id_str: Some(full.to_string()),
            name: Some(rec.name.clone()),
            legacy: None,
            all: false,
            attached: rec.legacy_slugs.clone(),
            warning,
            adopted: 0,
            moved: false,
        }
    }
}

/// Last path component of a canonical root, sanitized to registry-name
/// shape. Empty roots and filesystem roots yield `None`.
pub fn basename_slug(root: &str) -> Option<String> {
    let base = root.trim_end_matches('/').rsplit('/').next()?;
    if base.is_empty() {
        return None;
    }
    Some(ltmrs_domain::project_id::sanitize_name(base))
}

/// A path counts as a live checkout while it exists on the filesystem.
fn path_exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

/// Remediation named by every shadow refusal.
pub const BIND_REMEDIATION: &str = "run `ltmrs project bind --here` (or `--root DIR`) in this checkout to inspect or adopt its identity";

/// Shared-visibility note when an adopted slug is attached elsewhere.
fn shared_warning(slug: &str, others: &[String]) -> Option<String> {
    if others.is_empty() {
        None
    } else {
        Some(format!(
            "legacy slug '{slug}' is also attached to {}; visibility is shared",
            others.join(", ")
        ))
    }
}

/// One registered stable identity: ULID bits plus the human-readable name,
/// every canonical root seen under it, and explicitly attached legacy slugs.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProjectRecord {
    pub id: EntityId,
    pub name: String,
    pub canonical_roots: Vec<String>,
    pub first_seen: u64,
    pub last_seen: u64,
    pub legacy_slugs: Vec<String>,
}

/// Storage key for a project record.
pub fn project_key(id: EntityId) -> String {
    format!("prj:{}", id.as_uuid())
}

/// Storage key for the secondary root → id index.
pub fn root_key(root: &str) -> String {
    format!("root:{root}")
}

/// What the daemon needs to resolve one call: the winning stable id (if
/// any), the frontend-canonicalized root, and the wire legacy project.
/// `legacy_explicit` is true when the raw tool args carried a `project` key
/// (even `"global"`); otherwise the daemon falls back to the cwd clue.
pub struct IdentityClaim<'a> {
    pub project_id: Option<&'a str>,
    pub canonical_root: Option<&'a str>,
    pub legacy_project: Option<&'a str>,
    pub legacy_explicit: bool,
    pub all: bool,
}

/// The resolved scope for one call: stable identity (when present),
/// legacy fallback, cross-project flag, attachment for eligibility, and
/// user-facing notes. Writes additionally report adoption/move side
/// effects; reads never mutate.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedScope {
    pub project_id: Option<EntityId>,
    pub project_id_str: Option<String>,
    pub name: Option<String>,
    pub legacy: Option<String>,
    pub all: bool,
    pub attached: Vec<String>,
    pub warning: Option<String>,
    pub adopted: usize,
    pub moved: bool,
}

impl ResolvedScope {
    /// Fully unattributed scope (no identity, no legacy, no cross-project):
    /// global-only recall, legacy compat behavior.
    pub fn unattributed() -> Self {
        Self {
            project_id: None,
            project_id_str: None,
            name: None,
            legacy: None,
            all: false,
            attached: vec![],
            warning: None,
            adopted: 0,
            moved: false,
        }
    }
}

/// Outcome of an explicit legacy-slug adoption.
#[derive(Debug, Clone, PartialEq)]
pub struct AdoptOutcome {
    pub record: ProjectRecord,
    pub attached: usize,
    pub already_attached: bool,
    pub created: bool,
    pub warning: Option<String>,
}

/// Outcome of a write-time identity claim.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimOutcome {
    pub record: ProjectRecord,
    pub created: bool,
    pub moved: bool,
    pub adopted: usize,
    pub collision_warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltmrs_domain::command::{DomainCommand, OperationScope};
    use ltmrs_domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
    use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
    use ltmrs_domain::project_id::generate_project_id;
    use uuid::Uuid;

    fn repo() -> (CanonicalRepository, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        let r = CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
        (r, dir)
    }

    fn claim<'a>(
        id: Option<&'a str>,
        root: Option<&'a str>,
        legacy: Option<&'a str>,
        all: bool,
    ) -> IdentityClaim<'a> {
        IdentityClaim {
            project_id: id,
            canonical_root: root,
            legacy_project: legacy,
            legacy_explicit: legacy.is_some(),
            all,
        }
    }

    fn eid(n: u128) -> EntityId {
        EntityId::new(Uuid::from_u128(n))
    }

    fn ctx(n: u64) -> ltmrs_domain::command::CommandContext {
        ltmrs_domain::command::CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            session: None,
            operation_id: OperationId::new(Uuid::from_u128(n as u128)),
            request_digest: format!("d{n}"),
            deadline_millis: None,
            scope: Default::default(),
            retry_epoch: 1,
        }
    }

    fn memory(n: u64, project: Option<&str>, project_id: Option<EntityId>) -> Memory {
        Memory {
            id: eid(n as u128),
            external_alias: None,
            title: format!("t{n}"),
            fragment: format!("f{n}"),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: project.map(|s| s.to_string()),
            project_id,
            source: MemorySource::Ai,
            confidence: 0.5,
            quality_score: None,
            lifecycle: MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: ltmrs_domain::id::EntityRevision::new(1),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(1),
            created_at: Instant::new(100),
            updated_at: Instant::new(100),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    fn write_mem(repo: &CanonicalRepository, n: u64, op: u64) {
        let ns = repo
            .issue_namespace(
                FrontendId::new(Uuid::from_u128(1)),
                ChannelId::new(Uuid::from_u128(2)),
                1000,
            )
            .unwrap_or_else(|_| {
                repo.lookup_namespace(FrontendId::new(Uuid::from_u128(1)), 1)
                    .unwrap()
                    .unwrap()
            });
        let mut c = ctx(op);
        c.retry_epoch = ns.retry_epoch;
        repo.apply(
            &c,
            &DomainCommand::AddMemory {
                memory: memory(n, None, None),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }

    /// First-use persistence: an unknown id registers with its root and
    /// attaches the same-name legacy bucket, reporting the adopted count.
    #[test]
    fn unknown_id_registers_with_legacy_attach() {
        let (r, _d) = repo();
        write_mem(&r, 1, 101);
        // Name the legacy row "shop" via direct rewrite (fixture brevity).
        let mut m = r.get_memories(&[eid(1)]).unwrap().remove(0);
        m.project = Some("shop".into());
        // (Rewrite through the validated test seam.)
        let ns = r
            .issue_namespace(
                FrontendId::new(Uuid::from_u128(9)),
                ChannelId::new(Uuid::from_u128(9)),
                1000,
            )
            .unwrap();
        let mut c = ctx(102);
        c.retry_epoch = ns.retry_epoch;
        c.frontend_id = FrontendId::new(Uuid::from_u128(9));
        c.channel_id = ChannelId::new(Uuid::from_u128(9));
        r.apply(
            &c,
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch: ltmrs_domain::command::MemoryPatch {
                    project: Some(Some("shop".into())),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        let (full, id) = generate_project_id("shop");
        let out = r
            .resolve_identity(
                &claim(Some(&full), Some("/tmp/x/shop"), Some("shop"), false),
                true,
            )
            .unwrap();
        assert_eq!(out.project_id, Some(id));
        assert_eq!(out.attached, vec!["shop".to_string()]);
        assert_eq!(out.adopted, 1);
        let rec = r.project_record(id).unwrap().unwrap();
        assert_eq!(rec.legacy_slugs, vec!["shop".to_string()]);
        assert_eq!(rec.canonical_roots, vec!["/tmp/x/shop".to_string()]);
    }

    /// Two same-basename checkouts isolate: A's writes are invisible to B's
    /// scope and vice versa.
    #[test]
    fn two_same_basename_checkouts_isolate() {
        let (r, _d) = repo();
        let (full_a, id_a) = generate_project_id("shop");
        let (full_b, id_b) = generate_project_id("shop");
        assert_ne!(id_a, id_b);
        let a = r
            .resolve_identity(
                &claim(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), false),
                true,
            )
            .unwrap();
        assert!(
            a.warning.is_none(),
            "first registration has nothing to collide with"
        );
        let b = r
            .resolve_identity(
                &claim(Some(&full_b), Some("/tmp/b/shop"), Some("shop"), false),
                true,
            )
            .unwrap();
        assert!(
            b.warning.is_some(),
            "second same-name registration must warn"
        );
        assert_eq!(a.project_id, Some(id_a));
        assert_eq!(b.project_id, Some(id_b));

        // Reads under each identity see only their own scope.
        let ra = r
            .resolve_identity(
                &claim(Some(&full_a), Some("/tmp/a/shop"), None, false),
                false,
            )
            .unwrap();
        let rb = r
            .resolve_identity(
                &claim(Some(&full_b), Some("/tmp/b/shop"), None, false),
                false,
            )
            .unwrap();
        assert_eq!(ra.project_id, Some(id_a));
        assert_eq!(rb.project_id, Some(id_b));
        assert_ne!(ra.project_id, rb.project_id);
    }

    /// (a) A root registered to A but claimed by B refuses writes and names
    /// both candidates; explicit reads proceed with a warning.
    #[test]
    fn shadow_a_root_claimed_by_other_id() {
        let (r, _d) = repo();
        let (full_a, _id_a) = generate_project_id("shop");
        r.resolve_identity(
            &claim(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), false),
            true,
        )
        .unwrap();
        // B is a real identity registered at its own checkout.
        let (full_b, _id_b) = generate_project_id("other");
        r.resolve_identity(
            &claim(Some(&full_b), Some("/tmp/b/other"), Some("other"), false),
            true,
        )
        .unwrap();
        // Claiming B from A's bound root refuses writes, naming both.
        let err = r
            .resolve_identity(
                &claim(Some(&full_b), Some("/tmp/a/shop"), Some("other"), false),
                true,
            )
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
        assert!(
            err.message.contains(&full_a),
            "names owner, got: {}",
            err.message
        );
        assert!(err.message.contains("project bind"), "got: {}", err.message);
        // Explicit reads proceed with a warning naming the ambiguity.
        let ro = r
            .resolve_identity(
                &claim(Some(&full_b), Some("/tmp/a/shop"), Some("other"), false),
                false,
            )
            .unwrap();
        assert!(ro.warning.is_some());
    }

    /// (b) A checkout bound to Y claiming an unknown same-name stranger Z
    /// refuses (likely a copied bind file), naming the bound identity.
    #[test]
    fn shadow_b_bound_root_claiming_stranger() {
        let (r, _d) = repo();
        let (full_y, _id_y) = generate_project_id("shop");
        r.resolve_identity(
            &claim(Some(&full_y), Some("/tmp/b/shop"), Some("shop"), false),
            true,
        )
        .unwrap();
        let (full_z, _id_z) = generate_project_id("shop");
        let err = r
            .resolve_identity(
                &claim(Some(&full_z), Some("/tmp/b/shop"), Some("shop"), false),
                true,
            )
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
        assert!(err.message.contains(&full_y), "got: {}", err.message);
    }

    /// (c) No id + cwd basename matching two ids: writes refuse, reads
    /// refuse without all=true, proceed with a warning under all=true.
    #[test]
    fn shadow_c_basename_matches_two_ids() {
        let (r, _d) = repo();
        let (full_a, _a) = generate_project_id("shop");
        let (full_b, _b) = generate_project_id("shop");
        for (full, root) in [(&full_a, "/tmp/a/shop"), (&full_b, "/tmp/b/shop")] {
            r.resolve_identity(&claim(Some(full), Some(root), Some("shop"), false), true)
                .unwrap();
        }
        let w = r.resolve_identity(&claim(None, Some("/tmp/c/shop"), None, false), true);
        let err = w.unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
        assert!(err.message.contains("project bind"), "got: {}", err.message);
        let rd = r.resolve_identity(&claim(None, Some("/tmp/c/shop"), None, false), false);
        assert_eq!(rd.unwrap_err().code, DomainErrorCode::Validation);
        let all = r
            .resolve_identity(&claim(None, Some("/tmp/c/shop"), None, true), false)
            .unwrap();
        assert!(all.all);
        assert!(all.warning.is_some());
    }

    /// (c) No id + basename matching one id whose roots exclude this cwd:
    /// writes refuse; all=true reads proceed with a warning.
    #[test]
    fn shadow_c_single_match_excluding_root() {
        let (r, _d) = repo();
        let (full_a, _a) = generate_project_id("shop");
        r.resolve_identity(
            &claim(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), false),
            true,
        )
        .unwrap();
        let err = r
            .resolve_identity(&claim(None, Some("/tmp/other/shop"), None, false), true)
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
        assert!(err.message.contains(&full_a), "got: {}", err.message);
        let all = r
            .resolve_identity(&claim(None, Some("/tmp/other/shop"), None, true), false)
            .unwrap();
        assert!(all.warning.is_some());
    }

    /// (d) A legacy string attached to two identities: legacy reads refuse,
    /// all=true proceeds with a warning; stable reads are unaffected.
    #[test]
    fn shadow_d_legacy_string_matches_two_buckets() {
        let (r, _d) = repo();
        let (full_a, id_a) = generate_project_id("shop");
        let (full_b, _id_b) = generate_project_id("shop");
        for (full, root) in [(&full_a, "/tmp/a/shop"), (&full_b, "/tmp/b/shop")] {
            r.resolve_identity(&claim(Some(full), Some(root), Some("shop"), false), true)
                .unwrap();
        }
        assert_eq!(r.legacy_bucket_names("shop").unwrap().len(), 2);
        let err = r
            .resolve_identity(&claim(None, None, Some("shop"), false), false)
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
        // all=true proceeds across buckets with a warning naming them.
        let all = r
            .resolve_identity(&claim(None, None, Some("shop"), true), false)
            .unwrap();
        assert!(all.all);
        assert!(all.warning.is_some());
        // Stable reads under either identity are unaffected.
        let stable = r
            .resolve_identity(
                &claim(Some(&full_a), Some("/tmp/a/shop"), None, false),
                false,
            )
            .unwrap();
        assert_eq!(stable.project_id, Some(id_a));
        assert!(stable.warning.is_none());
    }

    /// Unknown explicit ids never resolve on reads (likely a typo or an
    /// unregistered bind): refusal names same-name candidates.
    #[test]
    fn unknown_id_read_refused_with_candidates() {
        let (r, _d) = repo();
        let (full_a, _a) = generate_project_id("shop");
        r.resolve_identity(
            &claim(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), false),
            true,
        )
        .unwrap();
        let (full_z, _z) = generate_project_id("shop");
        let err = r
            .resolve_identity(
                &claim(Some(&full_z), Some("/tmp/z/shop"), None, false),
                false,
            )
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
        assert!(err.message.contains(&full_a), "got: {}", err.message);
    }

    /// A moved checkout (old root gone) appends the new root and reports it.
    #[test]
    fn moved_checkout_root_update() {
        let (r, dir) = repo();
        let old = dir.path().join("old").join("shop");
        std::fs::create_dir_all(&old).unwrap();
        let old_s = old.to_str().unwrap().to_string();
        let (full, id) = generate_project_id("shop");
        r.resolve_identity(&claim(Some(&full), Some(&old_s), Some("shop"), false), true)
            .unwrap();
        std::fs::remove_dir_all(dir.path().join("old")).unwrap();
        assert!(!std::path::Path::new(&old_s).exists());
        let new = dir.path().join("new").join("shop");
        std::fs::create_dir_all(&new).unwrap();
        let new_s = new.to_str().unwrap().to_string();
        let out = r
            .resolve_identity(&claim(Some(&full), Some(&new_s), Some("shop"), false), true)
            .unwrap();
        assert!(out.moved);
        assert!(out.warning.is_some());
        let rec = r.project_record(id).unwrap().unwrap();
        assert!(rec.canonical_roots.contains(&new_s));
    }

    /// Sessions carry the resolved stable identity.
    #[test]
    fn session_stores_resolved_project_id() {
        use ltmrs_domain::session::SessionOp;
        let (r, _d) = repo();
        let (full, id) = generate_project_id("shop");
        let out = r
            .resolve_identity(
                &claim(Some(&full), Some("/tmp/s/shop"), Some("shop"), false),
                true,
            )
            .unwrap();
        assert_eq!(out.project_id, Some(id));
        let scope = OperationScope {
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            retry_epoch: r
                .issue_namespace(
                    FrontendId::new(Uuid::from_u128(1)),
                    ChannelId::new(Uuid::from_u128(2)),
                    1000,
                )
                .unwrap()
                .retry_epoch,
            operation_id: OperationId::new(Uuid::from_u128(7)),
            request_digest: "sess".to_string(),
        };
        let handle = ltmrs_domain::id::SessionHandle::new(Uuid::from_u128(77));
        match r
            .session_start_full_tx(
                &scope,
                handle,
                Some("shop".into()),
                out.project_id,
                None,
                vec![],
                None,
                None,
                crate::repository::SessionStartEffects::default(),
                1000,
            )
            .unwrap()
        {
            SessionOp::Applied(h) => assert_eq!(h, handle),
            other => panic!("expected Applied, got {other:?}"),
        }
        let s = r.get_session(handle).unwrap().unwrap();
        assert_eq!(s.project_id, Some(id));
        assert_eq!(s.project.as_deref(), Some("shop"));
    }

    /// Adopt under a renamed identity names the registered name, like the
    /// claimed-id refusal does — never the full id twice.
    #[test]
    fn adopt_renamed_identity_names_registered_name() {
        use ltmrs_domain::project_id::generate_project_id;
        let (r, _d) = repo();
        let (_full_a, id_a) = generate_project_id("shop");
        r.claim_project_for_write(id_a, "shop", Some("/tmp/a/shop"), Some("shop"), 1000)
            .unwrap();
        let err = r
            .adopt_legacy_slug(id_a, "wrong", Some("/tmp/a/shop"), "other")
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
        assert!(
            err.message.contains("registered as 'shop'"),
            "must name the registered name, got: {}",
            err.message
        );
    }
}
