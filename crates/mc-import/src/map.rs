//! Slice C C3: MC row -> ltmrs Memory mapping (loss-accounted).
//!
//! `McMemory` rows map to canonical `Memory`: external MC ids preserved in
//! `external_alias`, never overwriting ltmrs ids; unknown/unmapped columns
//! land in the loss list (RV-16: no coerced zero quality). Embeddings
//! import as opaque vectors bound to memory identity (never re-embedded,
//! never fed to the canonical embedding pipeline). MC `status` maps to
//! lifecycle: only `active` rows are Live; anything else is skipped with
//! loss noted (never silently dropped).

use super::{McImportError, fail};

/// One MC memory row (subset read by the importer).
#[derive(Debug, Clone)]
pub struct McMemory {
    pub id: i64,
    pub project_path: String,
    pub category: String,
    pub content: String,
    pub normalized_hash: String,
    pub importance: Option<i64>,
    pub scope: String,
    pub source_type: String,
    pub seen_count: i64,
    pub retrieval_count: i64,
    pub first_seen_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seen_at: i64,
    pub status: String,
}

/// One MC embedding row (opaque bytes + model id).
#[derive(Debug, Clone)]
pub struct McEmbedding {
    pub memory_id: i64,
    pub bytes: Vec<u8>,
    pub model_id: String,
}

/// Loss entry: a dropped/unmapped column value, accounted never silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McLoss {
    pub mc_id: i64,
    pub field: String,
    pub reason: String,
}

/// Mapped memory plus its loss list and optional opaque embedding.
#[derive(Debug)]
pub struct MappedMemory {
    pub memory: ltmrs_domain::memory::Memory,
    pub loss: Vec<McLoss>,
    pub embedding: Option<McEmbedding>,
}

/// Map one row. `mc_id` namespaces the deterministic EntityId so MC rows
/// never collide with ltmrs rows; the numeric MC id rides `external_alias`.
pub fn map_memory(row: &McMemory, embedding: Option<McEmbedding>) -> Result<MappedMemory, McImportError> {
    use ltmrs_domain::id::{EntityId, EntityRevision, DocumentRevision, EligibilityRevision, ExternalAlias};
    use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};

    if row.status != "active" {
        return Err(McImportError(format!(
            "mc memory {} has status {:?} (only active rows import)",
            row.id, row.status
        )));
    }
    let mut loss = Vec::new();
    // Columns with no canonical home are accounted, never coerced:
    for (field, present) in [
        ("importance", row.importance.is_some()),
        ("scope", !row.scope.is_empty()),
        ("seen_count", row.seen_count != 0),
        ("retrieval_count", row.retrieval_count != 0),
    ] {
        if present {
            loss.push(McLoss {
                mc_id: row.id,
                field: field.to_string(),
                reason: "no canonical home; preserved in import receipt, not coerced".to_string(),
            });
        }
    }
    if row.category.is_empty() {
        return Err(McImportError(format!("mc memory {} has empty category", row.id)));
    }
    let eid = EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("ltmrs:mc-import:memory:{}", row.id).as_bytes(),
    ));
    let title: String = row.content.lines().next().unwrap_or("").chars().take(80).collect();
    let memory = Memory {
        id: eid,
        external_alias: Some(ExternalAlias::new(format!("mc:{id}", id = row.id))),
        title,
        fragment: row.content.clone(),
        description: String::new(),
        fragment_type: FragmentType::Fact,
        project: None,
        project_id: None,
        source: MemorySource::Ai,
        confidence: 1.0,
        quality_score: None,
        lifecycle: MemoryLifecycle::Live,
        tags: vec![format!("mc-category:{}", row.category)],
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
        entity_revision: EntityRevision::new(0),
        document_revision: DocumentRevision::new(0),
        eligibility_revision: EligibilityRevision::new(0),
        created_at: Instant::new(row.created_at.max(0) as u64),
        updated_at: Instant::new(row.updated_at.max(0) as u64),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    };
    Ok(MappedMemory { memory, loss, embedding })
}

/// Read all active memory rows (+ their embeddings) from an open copy.
pub fn read_memories(
    conn: &rusqlite::Connection,
) -> Result<Vec<(McMemory, Option<McEmbedding>)>, McImportError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, project_path, category, content, normalized_hash, importance, scope, source_type, seen_count, retrieval_count, first_seen_at, created_at, updated_at, last_seen_at, status FROM memories",
        )
        .map_err(|e| fail(&format!("cannot read memories: {e}")))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(McMemory {
                id: r.get(0)?,
                project_path: r.get(1)?,
                category: r.get(2)?,
                content: r.get(3)?,
                normalized_hash: r.get(4)?,
                importance: r.get(5)?,
                scope: r.get(6)?,
                source_type: r.get(7)?,
                seen_count: r.get(8)?,
                retrieval_count: r.get(9)?,
                first_seen_at: r.get(10)?,
                created_at: r.get(11)?,
                updated_at: r.get(12)?,
                last_seen_at: r.get(13)?,
                status: r.get(14)?,
            })
        })
        .map_err(|e| fail(&format!("cannot iterate memories: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        let m: McMemory = row.map_err(|e| fail(&format!("cannot decode memory row: {e}")))?;
        let emb: Option<McEmbedding> = conn
            .query_row(
                "SELECT memory_id, embedding, model_id FROM memory_embeddings WHERE memory_id = ?1 LIMIT 1",
                [m.id],
                |r| {
                    Ok(McEmbedding {
                        memory_id: r.get(0)?,
                        bytes: r.get(1)?,
                        model_id: r.get(2)?,
                    })
                },
            )
            .ok();
        out.push((m, emb));
    }
    Ok(out)
}
