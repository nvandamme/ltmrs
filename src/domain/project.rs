//! Project identity type.

use crate::domain::id::EntityId;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Project {
    pub id: EntityId,
    pub name: String,
    pub legacy_name: Option<String>,
    pub is_global: bool,
}

impl Project {
    pub fn is_global_scope(&self) -> bool {
        self.is_global
    }
}
