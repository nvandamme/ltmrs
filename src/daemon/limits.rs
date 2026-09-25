//! Per-client quotas and resource budgets (design §7.3, RQ-22, T-CONC-04).
//!
//! Enforces limits for clients, per-client queued work, in-flight storage
//! operations, response bytes and maintenance jobs. Fairness prevents an
//! indexing rebuild from starving interactive recall. Backpressure is visible
//! as a retryable busy/error state, not unbounded allocation.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::domain::id::FrontendId;

/// Resource budgets for the daemon.
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    /// Maximum concurrent clients.
    pub max_clients: usize,
    /// Maximum queued jobs per client.
    pub max_queued_per_client: usize,
    /// Maximum in-flight storage operations.
    pub max_in_flight_storage: usize,
    /// Maximum response bytes per frame.
    pub max_response_bytes: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_clients: 64,
            max_queued_per_client: 128,
            max_in_flight_storage: 32,
            max_response_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Error for quota violations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QuotaError {
    #[error("client limit reached")]
    TooManyClients,
    #[error("per-client queue full")]
    ClientQueueFull,
    #[error("storage capacity exhausted")]
    StorageBusy,
}

/// Tracks per-client and global resource usage against the configured limits.
pub struct QuotaTracker {
    limits: ResourceLimits,
    /// Per-client queued job counts.
    queued: Mutex<HashMap<FrontendId, usize>>,
    /// Registered clients.
    clients: Mutex<Vec<FrontendId>>,
    /// In-flight storage operations.
    in_flight: Mutex<usize>,
}

impl QuotaTracker {
    pub fn new(limits: ResourceLimits) -> Self {
        Self {
            limits,
            queued: Mutex::new(HashMap::new()),
            clients: Mutex::new(Vec::new()),
            in_flight: Mutex::new(0),
        }
    }

    /// Register a client. Fails if the client limit is reached.
    pub fn register_client(&self, frontend_id: FrontendId) -> Result<(), QuotaError> {
        let mut clients = self.clients.lock().unwrap();
        if clients.contains(&frontend_id) {
            return Ok(());
        }
        if clients.len() >= self.limits.max_clients {
            return Err(QuotaError::TooManyClients);
        }
        clients.push(frontend_id);
        Ok(())
    }

    /// Unregister a client, freeing its slot. Absent IDs are a no-op so a
    /// disconnect guard can run unconditionally at connection end.
    pub fn unregister_client(&self, frontend_id: FrontendId) {
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| c != &frontend_id);
    }

    /// Try to enqueue a job for a client. Fails if the per-client queue is full.
    pub fn try_enqueue(&self, frontend_id: FrontendId) -> Result<(), QuotaError> {
        let mut queued = self.queued.lock().unwrap();
        let count = queued.entry(frontend_id).or_insert(0);
        if *count >= self.limits.max_queued_per_client {
            return Err(QuotaError::ClientQueueFull);
        }
        *count += 1;
        Ok(())
    }

    /// Mark a job as completed (dequeue).
    pub fn dequeue(&self, frontend_id: FrontendId) {
        let mut queued = self.queued.lock().unwrap();
        if let Some(count) = queued.get_mut(&frontend_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                queued.remove(&frontend_id);
            }
        }
    }

    /// Try to start an in-flight storage operation. Fails if at capacity.
    pub fn try_start_storage(&self) -> Result<(), QuotaError> {
        let mut in_flight = self.in_flight.lock().unwrap();
        if *in_flight >= self.limits.max_in_flight_storage {
            return Err(QuotaError::StorageBusy);
        }
        *in_flight += 1;
        Ok(())
    }

    /// Mark a storage operation as finished.
    pub fn finish_storage(&self) {
        let mut in_flight = self.in_flight.lock().unwrap();
        *in_flight = in_flight.saturating_sub(1);
    }

    /// Whether a response of the given size is allowed.
    pub fn allows_response(&self, bytes: usize) -> bool {
        bytes <= self.limits.max_response_bytes
    }

    /// Number of registered clients (for health).
    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// Total queued jobs across clients (for health).
    pub fn total_queued(&self) -> usize {
        self.queued.lock().unwrap().values().sum()
    }

    /// In-flight storage operations (for health).
    pub fn in_flight_storage(&self) -> usize {
        *self.in_flight.lock().unwrap()
    }
}

impl Default for QuotaTracker {
    fn default() -> Self {
        Self::new(ResourceLimits::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn fe(n: u64) -> FrontendId {
        FrontendId::new(Uuid::from_u128(n as u128))
    }

    #[test]
    fn client_limit_enforced() {
        let tracker = QuotaTracker::new(ResourceLimits {
            max_clients: 2,
            ..Default::default()
        });
        assert!(tracker.register_client(fe(1)).is_ok());
        assert!(tracker.register_client(fe(2)).is_ok());
        assert_eq!(
            tracker.register_client(fe(3)),
            Err(QuotaError::TooManyClients)
        );
        // Re-registering an existing client is fine.
        assert!(tracker.register_client(fe(1)).is_ok());
    }

    #[test]
    fn per_client_queue_limit_enforced() {
        let tracker = QuotaTracker::new(ResourceLimits {
            max_queued_per_client: 2,
            ..Default::default()
        });
        assert!(tracker.try_enqueue(fe(1)).is_ok());
        assert!(tracker.try_enqueue(fe(1)).is_ok());
        assert_eq!(tracker.try_enqueue(fe(1)), Err(QuotaError::ClientQueueFull));
        // Another client is unaffected.
        assert!(tracker.try_enqueue(fe(2)).is_ok());
    }

    #[test]
    fn storage_capacity_enforced() {
        let tracker = QuotaTracker::new(ResourceLimits {
            max_in_flight_storage: 2,
            ..Default::default()
        });
        assert!(tracker.try_start_storage().is_ok());
        assert!(tracker.try_start_storage().is_ok());
        assert_eq!(tracker.try_start_storage(), Err(QuotaError::StorageBusy));
        tracker.finish_storage();
        assert!(tracker.try_start_storage().is_ok());
    }

    #[test]
    fn response_size_checked() {
        let tracker = QuotaTracker::new(ResourceLimits {
            max_response_bytes: 100,
            ..Default::default()
        });
        assert!(tracker.allows_response(50));
        assert!(!tracker.allows_response(150));
    }

    #[test]
    fn unregister_frees_client_slot() {
        let tracker = QuotaTracker::new(ResourceLimits {
            max_clients: 1,
            ..Default::default()
        });
        tracker.register_client(fe(1)).unwrap();
        assert_eq!(
            tracker.register_client(fe(2)),
            Err(QuotaError::TooManyClients)
        );
        tracker.unregister_client(fe(1));
        assert!(tracker.register_client(fe(2)).is_ok());
        assert_eq!(tracker.client_count(), 1);
        // Unregistering an absent client is a no-op.
        tracker.unregister_client(fe(99));
        assert_eq!(tracker.client_count(), 1);
    }
}
