//! Recall result cache (Slice G6b; omp `enhancedRecall` parity).
//!
//! Keyed on (query, scope digest, store generation, result limit): the
//! generation rides the key, so a write (which bumps the generation)
//! can never serve a stale hit — invalidation is structural, not hooked.
//! Entries older than `CACHE_TTL_SECS` never hit. Pure data structure;
//! the engine owns one behind a mutex.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Cache TTL: 5 minutes (omp `enhancedRecall` parity).
pub const CACHE_TTL_SECS: u64 = 300;

/// Cache key: everything that makes two recalls interchangeable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecallCacheKey {
    pub query: String,
    pub scope_digest: String,
    pub store_generation: u64,
    pub result_limit: usize,
}

#[derive(Debug, Clone)]
struct Entry<V: Clone> {
    value: V,
    inserted: Instant,
}

/// Bounded recall cache (cap 64 entries, oldest-evicted). Generic over
/// the stored value: the engine caches full `RetrievalResult`s.
pub struct RecallCache<V: Clone = Vec<ltmrs_domain::id::EntityId>> {
    entries: HashMap<RecallCacheKey, Entry<V>>,
}

impl<V: Clone> RecallCache<V> {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Look up a key: `Some` only when present and fresh.
    pub fn get(&self, key: &RecallCacheKey) -> Option<V> {
        let e = self.entries.get(key)?;
        if e.inserted.elapsed() > Duration::from_secs(CACHE_TTL_SECS) {
            return None;
        }
        Some(e.value.clone())
    }

    /// Store a result (evicts oldest past 64 entries).
    pub fn put(&mut self, key: RecallCacheKey, value: V) {
        if self.entries.len() >= 64
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.inserted)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(
            key,
            Entry {
                value,
                inserted: Instant::now(),
            },
        );
    }

    /// Explicit drop (mm/maintenance hooks; generation change already covers writes).
    pub fn invalidate(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<V: Clone> Default for RecallCache<V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// G6b RED: put/get round-trips fresh entries.
    #[test]
    fn cache_hit_when_fresh() {
        let mut c: RecallCache<Vec<ltmrs_domain::id::EntityId>> = RecallCache::new();
        let k = RecallCacheKey {
            query: "q".into(),
            scope_digest: "s".into(),
            store_generation: 1,
            result_limit: 10,
        };
        assert!(c.get(&k).is_none());
        c.put(k.clone(), vec![]);
        assert!(c.get(&k).is_some());
        assert_eq!(c.len(), 1);
    }

    /// G6b RED: invalidate drops everything.
    #[test]
    fn invalidate_clears() {
        let mut c: RecallCache<Vec<ltmrs_domain::id::EntityId>> = RecallCache::new();
        let k = RecallCacheKey {
            query: "q".into(),
            scope_digest: "s".into(),
            store_generation: 1,
            result_limit: 10,
        };
        c.put(k.clone(), vec![]);
        c.invalidate();
        assert!(c.get(&k).is_none());
    }
}
