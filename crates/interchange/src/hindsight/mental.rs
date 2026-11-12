//! Mental-model seed catalog + render cache (Slice G1c).
//!
//! omp parity (`hindsight/seeds.json`): three built-in seeds —
//! `user-preferences` (untagged, every project), `project-conventions` and
//! `project-decisions` (project-tagged). Seeds are create-only: existing
//! server-side models are never mutated by bootstrap.
//!
//! ltmrs owns the seed *catalog* (frozen data) and a render *cache*;
//! synthesis itself runs on the Hindsight server via `reflect`.

/// One seed definition (frozen; mirrors the omp seed entries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentalSeed {
    pub id: &'static str,
    pub name: &'static str,
    pub source_query: &'static str,
    pub project_tagged: bool,
    pub max_tokens: u32,
}

/// Built-in seed catalog (omp `seeds.json` parity, frozen).
pub const MENTAL_SEEDS: &[MentalSeed] = &[
    MentalSeed {
        id: "user-preferences",
        name: "User Preferences",
        source_query: "What does the user prefer in coding style, tooling, communication, and review? Capture only durable preferences expressed across sessions, not one-off requests.",
        project_tagged: false,
        max_tokens: 600,
    },
    MentalSeed {
        id: "project-conventions",
        name: "Project Conventions",
        source_query: "What are this project's conventions for code style, build, testing, release, and pull-request review? Only include conventions that are explicit in the project.",
        project_tagged: true,
        max_tokens: 800,
    },
    MentalSeed {
        id: "project-decisions",
        name: "Project Decisions",
        source_query: "What durable architectural or product decisions have been made for this project, and what rationale or trade-offs were recorded? Exclude transient plans and active task state.",
        project_tagged: true,
        max_tokens: 800,
    },
];

/// Seeds applicable to a scoping mode: tagged seeds only where banks carry
/// project tags (tagged mode); the untagged seed applies everywhere except
/// isolated per-project banks.
pub fn seeds_for_scope(mode: &super::Scoping) -> Vec<&'static MentalSeed> {
    MENTAL_SEEDS
        .iter()
        .filter(|s| match mode {
            super::Scoping::PerProjectTagged => true,
            super::Scoping::Global => !s.project_tagged,
            super::Scoping::PerProject => !s.project_tagged,
        })
        .collect()
}

/// Render one cached model block (`<mental_models>` injection shape).
/// Pure: empty content renders nothing (no empty sections injected).
pub fn render_mental_block(name: &str, content: &str) -> String {
    let body = content.trim();
    if body.is_empty() {
        return String::new();
    }
    format!("<mental_models>\n## {name}\n\n{body}\n</mental_models>")
}

/// Render the full injected block for all cached models. Pure.
pub fn render_mental_models(models: &[(&str, &str)]) -> String {
    let blocks: Vec<String> = models
        .iter()
        .map(|(name, content)| render_mental_block(name, content))
        .filter(|b| !b.is_empty())
        .collect();
    if blocks.is_empty() {
        return String::new();
    }
    blocks.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// G1c RED: seed catalog has exactly the three omp seeds.
    #[test]
    fn seed_catalog_matches_omp() {
        assert_eq!(MENTAL_SEEDS.len(), 3);
        let ids: Vec<_> = MENTAL_SEEDS.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec![
                "user-preferences",
                "project-conventions",
                "project-decisions"
            ]
        );
        assert!(!MENTAL_SEEDS[0].project_tagged);
        assert!(MENTAL_SEEDS[1].project_tagged);
    }

    /// G1c RED: seed applicability per scoping mode.
    #[test]
    fn seeds_apply_per_mode() {
        use super::super::Scoping;
        assert_eq!(seeds_for_scope(&Scoping::PerProjectTagged).len(), 3);
        assert_eq!(seeds_for_scope(&Scoping::Global).len(), 1);
        assert_eq!(seeds_for_scope(&Scoping::PerProject).len(), 1);
    }

    /// G1c RED: render is pure; empties vanish.
    #[test]
    fn render_skips_empty() {
        assert!(render_mental_block("Prefs", "  ").is_empty());
        let out = render_mental_block("Prefs", "tabs over spaces");
        assert!(out.contains("<mental_models>") && out.contains("tabs over spaces"));
        assert!(render_mental_models(&[]).is_empty());
    }
}
