//! Identity helpers shared by tool adapters (moved verbatim from `tools.rs`):
//! legacy-ID mapping, ID resolution and deterministic new-ID minting.

use ltmrs_domain::command::DomainResult;
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::Memory;

use crate::envelope::IpcEnvelope;

pub(crate) fn legacy_id_of(
    repo: &ltmrs_service::repository::CanonicalRepository,
    m: &Memory,
) -> String {
    repo.legacy_id(m)
}

pub(crate) fn legacy_id_of_from_id(
    repo: &ltmrs_service::repository::CanonicalRepository,
    id: EntityId,
) -> String {
    repo.get_memories(&[id])
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|m| repo.legacy_id(&m))
        .unwrap_or_else(|| id.as_uuid().to_string())
}

pub(crate) fn resolve_id(
    repo: &ltmrs_service::repository::CanonicalRepository,
    id: &str,
) -> DomainResult<EntityId> {
    repo.resolve_id(id)
}

/// Legacy string ID for a new memory: derived deterministically from the
/// operation ID (unique per operation, stable across retries).
pub(crate) fn new_legacy_id(envelope: &IpcEnvelope) -> String {
    format!(
        "m{}",
        uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("ltmrs:memory:{}", envelope.operation_id.as_uuid()).as_bytes(),
        )
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect::<String>()
    )
}
