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

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn project_global_scope_reflects_flag() {
        let p = Project {
            id: EntityId::new(Uuid::from_u128(1)),
            name: "app".into(),
            legacy_name: Some("app".into()),
            is_global: false,
        };
        assert!(!p.is_global_scope());
        let g = Project {
            id: EntityId::new(Uuid::from_u128(2)),
            name: "global".into(),
            legacy_name: None,
            is_global: true,
        };
        assert!(g.is_global_scope());
    }
}
