//! Tiered recall shaping (Slice D D3): pure presentation-fullness tiers.
//!
//! `tier()` maps (age, importance, render budget) to a [`Tier`]. Monotone:
//! older or larger-over-budget never upgrades. No I/O, no clock —
//! `now_millis` is injected like `injection_score`.
//!
//! `render_budget` is a *render* budget (bytes for prompt), distinct from
//! service write/conflict budgets and the scan row-budget.

/// Presentation fullness for one recall item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Too old/large: omit entirely.
    Drop,
    /// Pointer only (id + title).
    Pointer,
    /// Truncated summary.
    Summary,
    /// Full fragment.
    Full,
}

/// Importance source (separately named from `injection_score` to avoid
/// coupling; formula confirmed in spec OQ-D1: confidence + access count).
pub fn recall_importance(confidence: f64, access_count: u64) -> f64 {
    let access = (access_count as f64).min(20.0) / 20.0;
    confidence.clamp(0.0, 1.0) * 0.7 + access * 0.3
}

/// Map (age, importance, budget) to a [`Tier`].
///
/// - `age_ms`: milliseconds since the item was created.
/// - `importance`: 0.0..=1.0 from [`recall_importance`].
/// - `budget_bytes`: remaining render budget for this item.
///
/// Rules (pinned by tests): importance < 0.2 → Drop; age > 180 days →
/// at most Pointer; budget < 64 bytes → Drop, < 256 → at most Pointer;
/// otherwise Full when importance ≥ 0.6, Summary when ≥ 0.35, Pointer.
pub fn tier(age_ms: u64, importance: f64, budget_bytes: u64) -> Tier {
    if budget_bytes < 64 || importance < 0.2 {
        return Tier::Drop;
    }
    const DAY: u64 = 86_400_000;
    let mut t = if importance >= 0.6 {
        Tier::Full
    } else if importance >= 0.35 {
        Tier::Summary
    } else {
        Tier::Pointer
    };
    if age_ms > 180 * DAY && t > Tier::Pointer {
        t = Tier::Pointer;
    }
    if budget_bytes < 256 && t > Tier::Pointer {
        t = Tier::Pointer;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D3: importance blends confidence (0.7) with capped access (0.3).
    #[test]
    fn importance_blends_confidence_and_access() {
        assert!((recall_importance(1.0, 0) - 0.7).abs() < 1e-9);
        assert!((recall_importance(1.0, 20) - 1.0).abs() < 1e-9);
        assert!(
            (recall_importance(1.0, 200) - 1.0).abs() < 1e-9,
            "access caps at 20"
        );
        assert!((recall_importance(0.0, 20) - 0.3).abs() < 1e-9);
    }

    /// D3: low importance always drops.
    #[test]
    fn low_importance_drops() {
        assert_eq!(tier(0, 0.1, 10_000), Tier::Drop);
        assert_eq!(tier(0, 0.19, 10_000), Tier::Drop);
    }

    /// D3: tiny budget drops or caps at pointer.
    #[test]
    fn tiny_budget_drops_or_pointers() {
        assert_eq!(tier(0, 0.9, 0), Tier::Drop);
        assert_eq!(tier(0, 0.9, 63), Tier::Drop);
        assert_eq!(tier(0, 0.9, 100), Tier::Pointer);
        assert_eq!(tier(0, 0.5, 100), Tier::Pointer);
    }

    /// D3: stale items cap at pointer.
    #[test]
    fn stale_items_cap_at_pointer() {
        const DAY: u64 = 86_400_000;
        assert_eq!(tier(200 * DAY, 0.9, 10_000), Tier::Pointer);
        assert_eq!(tier(200 * DAY, 0.5, 10_000), Tier::Pointer);
        assert_eq!(tier(10 * DAY, 0.9, 10_000), Tier::Full);
    }

    /// D3: mid importance summarizes.
    #[test]
    fn mid_importance_summarizes() {
        assert_eq!(tier(0, 0.6, 10_000), Tier::Full);
        assert_eq!(tier(0, 0.5, 10_000), Tier::Summary);
        assert_eq!(tier(0, 0.35, 10_000), Tier::Summary);
        assert_eq!(tier(0, 0.3, 10_000), Tier::Pointer);
    }

    /// D3: monotonicity — older or tighter never upgrades.
    #[test]
    fn tier_is_monotone_in_age_and_budget() {
        for &imp in &[0.3, 0.5, 0.9] {
            let mut last = Tier::Full;
            for &age in &[0, 10, 100, 200, 400] {
                let t = tier(age * 86_400_000, imp, 10_000);
                assert!(t <= last, "age {age}d upgraded {last:?} to {t:?}");
                last = t;
            }
            let mut last = Tier::Full;
            for &b in &[10_000u64, 1000, 200, 100, 10] {
                let t = tier(0, imp, b);
                assert!(t <= last, "budget {b} upgraded {last:?} to {t:?}");
                last = t;
            }
        }
    }
}
