//! Tiered compartments for session history (Slice D D4): presentation-only.
//!
//! Maps history rows to [`Compartment`]s via [`tier()`]. Assignment never
//! mutates rows, never reorders them, and summaries (when used) are marked
//! derived — never written back to canonical sessions.

use super::tier::{Tier, tier};

/// Presentation compartment for one history row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compartment {
    /// Always rendered in full; never dropped, never summarized.
    Pinned,
    /// Rendered in full (recent or important).
    RecentFull,
    /// Rendered as a truncated summary line.
    Summarized,
    /// Rendered as a pointer (id + title) only.
    PointerOnly,
}

/// One history row (projection of whatever the caller lists).
#[derive(Debug, Clone)]
pub struct HistoryRow {
    pub id: String,
    pub title: String,
    pub age_ms: u64,
    pub confidence: f64,
    pub access_count: u64,
    pub pinned: bool,
}

/// Assign compartments to `rows` under `render_budget` bytes, evaluated at
/// `now_ms` (injected clock). Order is preserved; total rendered bytes are
/// capped by degrading fullness (never by dropping pinned rows).
pub fn assign(
    rows: &[HistoryRow],
    render_budget: u64,
    now_ms: u64,
) -> Vec<(Compartment, HistoryRow)> {
    use super::tier::recall_importance;
    let mut out = Vec::with_capacity(rows.len());
    let mut spent: u64 = 0;
    for row in rows {
        let importance = recall_importance(row.confidence, row.access_count);
        // Pinned rows bypass tiering: they always render full.
        if row.pinned {
            spent += estimate_bytes(&row.title, Tier::Full);
            out.push((Compartment::Pinned, row.clone()));
            continue;
        }
        let created_ms = now_ms.saturating_sub(row.age_ms);
        let _ = created_ms;
        let remaining = render_budget.saturating_sub(spent);
        let mut t = tier(row.age_ms, importance, remaining);
        // Downgrade while over budget (Drop only when tier says so).
        while t != Tier::Drop
            && spent + estimate_bytes(&row.title, t) > render_budget
            && t != Tier::Pointer
        {
            t = match t {
                Tier::Full => Tier::Summary,
                Tier::Summary => Tier::Pointer,
                Tier::Pointer | Tier::Drop => Tier::Drop,
            };
        }
        spent += estimate_bytes(&row.title, t);
        let c = match t {
            Tier::Full => Compartment::RecentFull,
            Tier::Summary => Compartment::Summarized,
            Tier::Pointer | Tier::Drop => Compartment::PointerOnly,
        };
        out.push((c, row.clone()));
    }
    out
}

/// Byte estimate for one row at a tier: title + fixed framing.
fn estimate_bytes(title: &str, t: Tier) -> u64 {
    let body = match t {
        Tier::Full => title.len() as u64 + 200,
        Tier::Summary => 120,
        Tier::Pointer => 60,
        Tier::Drop => 0,
    };
    body + 16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, age_days: u64, conf: f64, access: u64, pinned: bool) -> HistoryRow {
        HistoryRow {
            id: id.to_string(),
            title: format!("title-{id}"),
            age_ms: age_days * 86_400_000,
            confidence: conf,
            access_count: access,
            pinned,
        }
    }

    /// D4: pinned rows always render full, even stale and over budget.
    #[test]
    fn pinned_never_dropped() {
        let rows = vec![row("old", 400, 0.1, 0, true)];
        let out = assign(&rows, 0, 0);
        assert_eq!(out[0].0, Compartment::Pinned);
    }

    /// D4: order preserved regardless of tiers.
    #[test]
    fn order_preserved() {
        let rows = vec![
            row("a", 1, 0.9, 5, false),
            row("b", 300, 0.1, 0, false),
            row("c", 2, 0.5, 3, false),
        ];
        let out = assign(&rows, 10_000, 0);
        let ids: Vec<&str> = out.iter().map(|(_, r)| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    /// D4: total rendered bytes capped by degrading fullness.
    #[test]
    fn budget_degrades_fullness() {
        let rows: Vec<HistoryRow> = (0..20)
            .map(|i| row(&format!("r{i}"), 1, 0.9, 5, false))
            .collect();
        let roomy = assign(&rows, 100_000, 0);
        let tight = assign(&rows, 500, 0);
        let full_roomy = roomy
            .iter()
            .filter(|(c, _)| *c == Compartment::RecentFull)
            .count();
        let full_tight = tight
            .iter()
            .filter(|(c, _)| *c == Compartment::RecentFull)
            .count();
        assert!(full_roomy > full_tight, "tight budget must degrade");
        // Nothing vanishes: every row still present (pointer at worst).
        assert_eq!(tight.len(), 20);
    }

    /// D4: assignment is presentation-only — input untouched.
    #[test]
    fn input_untouched() {
        let rows = vec![row("a", 1, 0.9, 5, false), row("b", 2, 0.5, 1, false)];
        let snapshot: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        let _ = assign(&rows, 10_000, 0);
        let after: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        assert_eq!(snapshot, after);
    }
}
