//! Deterministic clock and ID generation for testing.

use uuid::Uuid;

pub trait Clock {
    fn now_millis(&self) -> u64;
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
