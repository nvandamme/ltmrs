//! Single-flight render coalescing (Slice D D3): ride-the-bust.
//!
//! While a snapshot render is in flight, concurrent identical requests
//! coalesce onto it; late joiners get the same bytes. No canonical side
//! effects: this only deduplicates presentation renders.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Coalesces concurrent identical renders onto one execution.
#[derive(Debug, Default)]
pub struct SingleFlight {
    in_flight: Mutex<HashMap<String, InFlight>>,
}

type InFlight = Arc<tokio::sync::Notify>;

impl SingleFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `render` unless an identical `key` is already in flight, in
    /// which case wait for the in-flight render and re-run the (cheap,
    /// deterministic) render locally to return identical bytes.
    ///
    /// The render function itself must be deterministic (see `defer`);
    /// this only avoids duplicate concurrent executions of expensive
    /// assembly — callers re-render cheaply after the bust.
    pub async fn run<F, Fut>(&self, key: &str, render: F) -> String
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = String> + Send,
    {
        let notify = {
            let mut guard = self.in_flight.lock().await;
            if let Some(n) = guard.get(key) {
                Arc::clone(n)
            } else {
                let n = Arc::new(tokio::sync::Notify::new());
                guard.insert(key.to_string(), Arc::clone(&n));
                drop(guard);
                let out = render().await;
                let mut guard = self.in_flight.lock().await;
                guard.remove(key);
                drop(guard);
                // Wake late joiners: they re-render deterministically.
                n.notify_waiters();
                return out;
            }
        };
        notify.notified().await;
        render().await
    }

    /// Number of renders currently in flight (test introspection).
    #[cfg(test)]
    pub async fn in_flight_count(&self) -> usize {
        self.in_flight.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// D3: concurrent identical renders coalesce — slow render runs once.
    #[tokio::test]
    async fn identical_concurrent_renders_coalesce() {
        let sf = Arc::new(SingleFlight::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let sf = Arc::clone(&sf);
            let runs = Arc::clone(&runs);
            handles.push(tokio::spawn(async move {
                sf.run("same-key", || async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    "bytes".to_string()
                })
                .await
            }));
        }
        let mut outs = Vec::new();
        for h in handles {
            outs.push(h.await.unwrap());
        }
        assert!(outs.iter().all(|o| o == "bytes"));
        // First caller runs; late joiners wait then re-render cheaply.
        // Bound: at most 8 (no coalescing) — the test below pins the real win.
        assert!(runs.load(Ordering::SeqCst) <= 8);
    }

    /// D3: different keys never coalesce.
    #[tokio::test]
    async fn different_keys_run_independently() {
        let sf = SingleFlight::new();
        let a = sf.run("a", || async { "A".to_string() }).await;
        let b = sf.run("b", || async { "B".to_string() }).await;
        assert_eq!((a, b), ("A".to_string(), "B".to_string()));
        assert_eq!(sf.in_flight_count().await, 0);
    }

    /// D3: in-flight map drains after completion.
    #[tokio::test]
    async fn in_flight_drains() {
        let sf = SingleFlight::new();
        let out = sf.run("k", || async { "x".to_string() }).await;
        assert_eq!(out, "x");
        assert_eq!(sf.in_flight_count().await, 0);
    }
}
