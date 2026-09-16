//! Deterministic clock and ID generation for testing.

use uuid::Uuid;

pub trait Clock {
    fn now_millis(&self) -> u64;
}

/// Wall-clock time source for production use.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FrozenClock {
    current: u64,
}

impl FrozenClock {
    pub fn new(start: u64) -> Self {
        Self { current: start }
    }

    pub fn advance(&mut self, millis: u64) {
        self.current += millis;
    }

    pub fn now_millis(&self) -> u64 {
        self.current
    }
}

impl Clock for FrozenClock {
    fn now_millis(&self) -> u64 {
        self.current
    }
}

#[derive(Debug, Clone)]
pub struct DeterministicIdGen {
    entity_counter: u128,
    operation_counter: u128,
}

impl DeterministicIdGen {
    pub fn new(seed: u128) -> Self {
        Self {
            entity_counter: seed,
            operation_counter: seed + 1000,
        }
    }

    pub fn next_entity_id(&mut self) -> crate::domain::id::EntityId {
        self.entity_counter += 1;
        crate::domain::id::EntityId::new(Uuid::from_u128(self.entity_counter))
    }

    pub fn next_operation_id(&mut self) -> crate::domain::id::OperationId {
        self.operation_counter += 1;
        crate::domain::id::OperationId::new(Uuid::from_u128(self.operation_counter))
    }
}

#[derive(Debug, Clone)]
pub struct TestTimeSource {
    current: u64,
}

impl TestTimeSource {
    pub fn new(start: u64) -> Self {
        Self { current: start }
    }

    pub fn advance(&mut self, millis: u64) {
        self.current += millis;
    }

    pub fn now(&self) -> u64 {
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_ids_are_stable_and_unique() {
        let mut a = DeterministicIdGen::new(42);
        let mut b = DeterministicIdGen::new(42);
        let ids_a: Vec<_> = (0..5).map(|_| a.next_entity_id()).collect();
        let ids_b: Vec<_> = (0..5).map(|_| b.next_entity_id()).collect();
        assert_eq!(ids_a, ids_b, "same seed => same sequence");
        assert_eq!(
            ids_a.len(),
            ids_a
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
        );
    }

    #[test]
    fn frozen_clock_is_stable() {
        let c = FrozenClock::new(1000);
        assert_eq!(c.now_millis(), 1000);
        assert_eq!(c.now_millis(), 1000);
    }
}
