//! omp-compat status surfaces (P3): pure markdown renderers over existing
//! tool payloads. No IPC, no new tools — hosts call existing tools and
//! format locally. Mirrors omp `status/stats/diagnose/queuePreview`.

/// Render a backend status block (memory backend overview).
pub fn render_status(
    backend: &str,
    memories: usize,
    relations: usize,
    guides: usize,
    sessions: usize,
) -> String {
    format!(
        "## Memory backend: {backend}\n\n\
         - Memories: {memories}\n\
         - Relations: {relations}\n\
         - Guides: {guides}\n\
         - Sessions: {sessions}\n"
    )
}

/// Render memory statistics (shape mirrors `memory_stats` structured output).
pub fn render_stats(count: u64, avg_confidence: f64, recallable: u64) -> String {
    format!(
        "## Memory stats\n\n- Count: {count}\n- Avg confidence: {avg_confidence:.3}\n- Recallable: {recallable}\n"
    )
}

/// Render diagnostics: bridge state + snapshot health, honestly labeled.
pub fn render_diagnose(bridge: &str, snapshot_rows: usize, budget: usize) -> String {
    format!(
        "## Memory diagnostics\n\n- Bridge: {bridge}\n- Snapshot rows: {snapshot_rows}\n- Scan budget: {budget}\n"
    )
}

/// Render the pending retain queue depth (bridge outbox; 0 when disabled).
pub fn render_queue(pending: usize, max: usize) -> String {
    if pending == 0 {
        return "## Retain queue\n\nEmpty (nothing awaiting consolidation).\n".to_string();
    }
    format!("## Retain queue\n\n- Pending: {pending}/{max}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P3: golden pin — status rendering is byte-stable.
    #[test]
    fn status_renders_golden() {
        assert_eq!(
            render_status("ltmrs", 10, 3, 2, 1),
            "## Memory backend: ltmrs\n\n- Memories: 10\n- Relations: 3\n- Guides: 2\n- Sessions: 1\n"
        );
    }

    /// P3: golden pin — stats rendering with float formatting.
    #[test]
    fn stats_renders_golden() {
        assert_eq!(
            render_stats(42, 0.8755, 40),
            "## Memory stats\n\n- Count: 42\n- Avg confidence: 0.875\n- Recallable: 40\n"
        );
    }

    /// P3: golden pin — diagnose renders bridge state honestly.
    #[test]
    fn diagnose_renders_golden() {
        let out = render_diagnose("disabled (local-only)", 100, 10_000);
        assert!(out.contains("Bridge: disabled (local-only)"));
        assert!(out.contains("Snapshot rows: 100"));
    }

    /// P3: empty queue says empty; non-empty shows depth.
    #[test]
    fn queue_renders_depth() {
        assert!(render_queue(0, 256).contains("Empty"));
        assert!(render_queue(3, 256).contains("Pending: 3/256"));
    }
}
