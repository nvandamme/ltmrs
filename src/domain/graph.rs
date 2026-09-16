//! Graph invariants and predicates.

use std::collections::{HashMap, HashSet};

use crate::domain::command::DomainErrorCode;
use crate::domain::id::EntityId;
use crate::domain::relation::Relation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphValidation {
    Ok,
    Reject(DomainErrorCode),
}

pub fn validate_new_edge<'a>(
    new: &Relation,
    existing: impl IntoIterator<Item = &'a Relation>,
    live_memory_ids: impl Fn(EntityId) -> bool,
) -> GraphValidation {
    let existing: Vec<&'a Relation> = existing.into_iter().collect();
    if new.source == new.target && new.relation_type.is_supersession() {
        return GraphValidation::Reject(DomainErrorCode::SelfSupersession);
    }
    if !live_memory_ids(new.source) || !live_memory_ids(new.target) {
        return GraphValidation::Reject(DomainErrorCode::NotFound);
    }
    for e in &existing {
        if e.source == new.source && e.target == new.target && e.relation_type == new.relation_type
        {
            return GraphValidation::Reject(DomainErrorCode::DuplicateEdge);
        }
    }
    if new.relation_type.is_supersession()
        && forms_supersession_cycle(new, existing.iter().copied())
    {
        return GraphValidation::Reject(DomainErrorCode::SupersessionCycle);
    }
    GraphValidation::Ok
}

fn forms_supersession_cycle<'a>(
    new: &Relation,
    existing: impl IntoIterator<Item = &'a Relation>,
) -> bool {
    let mut adj: HashMap<EntityId, Vec<EntityId>> = HashMap::new();
    for e in existing {
        if e.relation_type.is_supersession() {
            adj.entry(e.source).or_default().push(e.target);
        }
    }
    adj.entry(new.source).or_default().push(new.target);

    let mut visited: HashSet<EntityId> = HashSet::new();
    let mut stack = vec![new.target];
    while let Some(node) = stack.pop() {
        if node == new.source {
            return true;
        }
        if !visited.insert(node) {
            continue;
        }
        if let Some(nexts) = adj.get(&node) {
            for n in nexts {
                stack.push(*n);
            }
        }
    }
    false
}

pub fn reverse_view(r: &Relation) -> Relation {
    r.reverse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::EntityId;
    use crate::domain::memory::Instant;
    use crate::domain::relation::RelationType;
    use uuid::Uuid;

    fn id(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }
    fn edge(s: u64, t: u64, ty: RelationType) -> Relation {
        Relation::new(id(1000 + s + t), id(s), id(t), ty, None, Instant::new(1))
    }
    fn all_live(_id: EntityId) -> bool {
        true
    }

    #[test]
    fn rejects_self_supersession() {
        let e = edge(1, 1, RelationType::Supersedes);
        assert_eq!(
            validate_new_edge(&e, std::iter::empty::<&Relation>(), all_live),
            GraphValidation::Reject(DomainErrorCode::SelfSupersession)
        );
    }

    #[test]
    fn rejects_missing_endpoint() {
        let e = edge(1, 2, RelationType::RelatedTo);
        assert_eq!(
            validate_new_edge(&e, std::iter::empty::<&Relation>(), |i| i != id(2)),
            GraphValidation::Reject(DomainErrorCode::NotFound)
        );
    }

    #[test]
    fn rejects_duplicate_edge() {
        let e = edge(1, 2, RelationType::Supports);
        let existing = [edge(1, 2, RelationType::Supports)];
        assert_eq!(
            validate_new_edge(&e, existing.iter(), all_live),
            GraphValidation::Reject(DomainErrorCode::DuplicateEdge)
        );
    }

    #[test]
    fn rejects_supersession_cycle() {
        let existing = [edge(1, 2, RelationType::Supersedes)];
        let new = edge(2, 1, RelationType::Supersedes);
        assert_eq!(
            validate_new_edge(&new, existing.iter(), all_live),
            GraphValidation::Reject(DomainErrorCode::SupersessionCycle)
        );
    }

    #[test]
    fn allows_valid_edge() {
        let e = edge(1, 2, RelationType::Supports);
        assert_eq!(
            validate_new_edge(&e, std::iter::empty::<&Relation>(), all_live),
            GraphValidation::Ok
        );
    }

    #[test]
    fn reverse_view_derived() {
        let e = edge(1, 2, RelationType::Supersedes);
        let r = reverse_view(&e);
        assert_eq!(r.source, id(2));
        assert_eq!(r.target, id(1));
        assert_eq!(r.relation_type, RelationType::SupersededBy);
    }
}
