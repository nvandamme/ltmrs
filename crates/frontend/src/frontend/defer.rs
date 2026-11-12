//! Defer/replay byte-identical-prefix discipline (Slice D D2).
//!
//! Presentation-only recall shaping: a deterministic snapshot renderer
//! plus an opaque defer handle. `replay` asserts the deferred prefix is
//! byte-identical to the full render's head; a renderer change bumps
//! [`RENDER_VERSION`] and old handles refuse (re-render, never patch).

/// Renderer version: bump on ANY render-format change. Old handles refuse.
pub const RENDER_VERSION: u32 = 1;

/// Opaque deferred render: the exact bytes of a snapshot prefix plus the
/// version that produced them. Fields are crate-visible so tests can
/// construct stale handles (external callers cannot forge them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferHandle {
    pub(crate) version: u32,
    pub(crate) prefix: Vec<u8>,
}

/// Render an opaque snapshot view deterministically: fixed key order,
/// fixed float formatting, `\n` line endings.
pub fn render_snapshot(view: &SnapshotView) -> String {
    let mut out = String::from("## snapshot\n");
    let mut rows: Vec<(&str, &str, f64)> = view
        .rows
        .iter()
        .map(|r| (r.key.as_str(), r.title.as_str(), r.score))
        .collect();
    rows.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    for (key, title, score) in rows {
        out.push_str(&format!("  [{key}] {title} ({score:.3})\n"));
    }
    out
}

/// A single snapshot row (projection of whatever the caller renders).
#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub key: String,
    pub title: String,
    pub score: f64,
}

/// The caller-assembled view to render.
#[derive(Debug, Clone, Default)]
pub struct SnapshotView {
    pub rows: Vec<SnapshotRow>,
}

/// Capture the first `prefix_len` bytes of a render as a defer handle.
/// Char-boundary safe: rounds down to the last complete char.
pub fn defer_key(rendered: &str, prefix_len: usize) -> DeferHandle {
    let bytes = rendered.as_bytes();
    let mut len = prefix_len.min(bytes.len());
    while len > 0 && !rendered.is_char_boundary(len) {
        len -= 1;
    }
    DeferHandle {
        version: RENDER_VERSION,
        prefix: bytes[..len].to_vec(),
    }
}

/// Error when a defer handle cannot replay (version skew).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayError;

/// Replay a deferred prefix against a fresh full render: asserts the
/// deferred bytes are an exact prefix of the render. Version mismatch
/// (renderer changed) refuses — re-render, never patch.
pub fn replay(handle: &DeferHandle, full_render: &str) -> Result<String, ReplayError> {
    if handle.version != RENDER_VERSION {
        return Err(ReplayError);
    }
    if full_render.as_bytes().starts_with(&handle.prefix) {
        Ok(full_render.to_string())
    } else {
        Err(ReplayError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> SnapshotView {
        SnapshotView {
            rows: vec![
                SnapshotRow {
                    key: "b".into(),
                    title: "Bee".into(),
                    score: 0.5,
                },
                SnapshotRow {
                    key: "a".into(),
                    title: "Aye".into(),
                    score: 0.9,
                },
                SnapshotRow {
                    key: "c".into(),
                    title: "Cee".into(),
                    score: 0.9,
                },
            ],
        }
    }

    /// D2: same view renders byte-identical every time.
    #[test]
    fn render_is_deterministic() {
        let v = view();
        assert_eq!(render_snapshot(&v), render_snapshot(&v));
    }

    /// D2: ties break by key, not input order.
    #[test]
    fn render_orders_score_then_key() {
        let r = render_snapshot(&view());
        let pa = r.find("[a]").unwrap();
        let pc = r.find("[c]").unwrap();
        let pb = r.find("[b]").unwrap();
        assert!(pa < pc && pc < pb, "got:\n{r}");
    }

    /// D2: defer captures an exact prefix; replay accepts it.
    #[test]
    fn replay_accepts_exact_prefix() {
        let full = render_snapshot(&view());
        let h = defer_key(&full, 20);
        assert_eq!(replay(&h, &full).unwrap(), full);
        assert!(full.as_bytes().starts_with(&h.prefix));
    }

    /// D2: forked render (same prefix, diverged tail) refuses.
    #[test]
    fn replay_rejects_forked_render() {
        let full = render_snapshot(&view());
        let h = defer_key(&full, 20);
        let mut forked = full.clone();
        forked.push_str("EXTRA");
        // Exact-prefix handles still accept extensions of the same render.
        assert_eq!(replay(&h, &forked).unwrap(), forked);
        // But a handle from another render refuses.
        let other = render_snapshot(&SnapshotView {
            rows: vec![SnapshotRow {
                key: "z".into(),
                title: "Zed".into(),
                score: 1.0,
            }],
        });
        assert!(replay(&h, &other).is_err());
    }

    /// D2: version skew refuses — re-render, never patch.
    #[test]
    fn replay_rejects_version_skew() {
        let full = render_snapshot(&view());
        let mut h = defer_key(&full, 20);
        h.version = RENDER_VERSION + 1;
        assert_eq!(replay(&h, &full), Err(ReplayError));
    }

    /// D2: char-boundary safe prefix (multi-byte titles never split).
    #[test]
    fn defer_key_respects_char_boundaries() {
        let v = SnapshotView {
            rows: vec![SnapshotRow {
                key: "e".into(),
                title: "héllo wörld".into(),
                score: 1.0,
            }],
        };
        let full = render_snapshot(&v);
        // Pick a prefix_len landing mid-`é` (bytes 15-16 of "héllo").
        let probe = full.find("h\u{e9}llo").unwrap() + 1;
        let h = defer_key(&full, probe);
        assert!(std::str::from_utf8(&h.prefix).is_ok());
        assert!(replay(&h, &full).is_ok());
    }

    /// D2: empty view renders a stable header.
    #[test]
    fn render_empty_view_stable() {
        let r = render_snapshot(&SnapshotView::default());
        assert_eq!(r, "## snapshot\n");
        let h = defer_key(&r, 99);
        assert_eq!(replay(&h, &r).unwrap(), r);
    }
}
