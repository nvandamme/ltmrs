//! Sequential reference interpreter: the oracle for concurrency histories.
//!
//! This is NOT a production backend candidate. It provides deterministic
//! semantics for testing concurrent operations against a serial reference.

use std::collections::HashMap;

use crate::clock::{DeterministicIdGen, FrozenClock};
use crate::command::ReceiptLedger;
use crate::guide::Guide;
use crate::id::{EntityId, ExternalAlias, SessionHandle, StoreGeneration};
use crate::memory::Memory;
use crate::relation::Relation;
use crate::session::{FeedbackEvent, Session};

mod apply;
#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod conc_tests;
#[cfg(test)]
mod guides_tests;
mod memory_ops;
#[cfg(test)]
mod oracle_tests;
mod registry;
mod session_guide_ops;
#[cfg(test)]
mod session_guide_tests;
#[cfg(test)]
mod test_support;

pub struct ReferenceInterpreter {
    pub store_generation: StoreGeneration,
    memories: HashMap<EntityId, Memory>,
    aliases: HashMap<ExternalAlias, EntityId>,
    guides: HashMap<String, Guide>,
    sessions: HashMap<SessionHandle, Session>,
    relations: Vec<Relation>,
    feedback: Vec<FeedbackEvent>,
    suggestions: Vec<crate::session::Suggestion>,
    receipts: ReceiptLedger,
    id_gen: DeterministicIdGen,
    clock: FrozenClock,
    revision_counter: u64,
}

impl ReferenceInterpreter {
    pub fn new(seed: u128, start_millis: u64) -> Self {
        Self {
            store_generation: StoreGeneration::FIRST,
            memories: HashMap::new(),
            aliases: HashMap::new(),
            guides: HashMap::new(),
            sessions: HashMap::new(),
            relations: Vec::new(),
            feedback: Vec::new(),
            suggestions: Vec::new(),
            receipts: ReceiptLedger::new(),
            id_gen: DeterministicIdGen::new(seed),
            clock: FrozenClock::new(start_millis),
            revision_counter: 0,
        }
    }
}
