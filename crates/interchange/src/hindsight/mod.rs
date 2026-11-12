//! Hindsight bridge (Slice A): live dual-write + scoped recall.
//!
//! ltmrs stays authoritative: the bridge is best-effort additive only.
//! See `precedent.rs` for the frozen wire table.

pub mod client;
pub mod mental;
pub mod outbox;
pub mod precedent;
pub mod types;

pub use mental::{
    MENTAL_SEEDS, MentalSeed, render_mental_block, render_mental_models, seeds_for_scope,
};
pub use outbox::{
    dest_dir, event_filename, outbox_expiry_millis, outbox_root, quarantine_cutoff_millis,
    quarantine_dir, write_event,
};
pub use types::{
    BridgeError, HindsightConfig, RecallHit, RetainDoc, document_id, hindsight_config_from_env,
};
/// Tags for one retained doc: configured defaults + per-doc scope tags.
/// Pure (no I/O) so tag derivation is unit-testable without a server.
pub fn tags_for_scope(
    config: &HindsightConfig,
    project: Option<&str>,
    channel: Option<&str>,
) -> Vec<String> {
    let mut tags = config.default_tags.clone();
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        tags.push(format!("project:{p}"));
    }
    if let Some(c) = channel.filter(|c| !c.is_empty()) {
        tags.push(format!("channel:{c}"));
    }
    tags.sort();
    tags.dedup();
    tags
}

/// Bank scoping mode (omp `hindsight.scoping` parity; default tagged).
/// Pure and total: every mode × project combination yields a bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scoping {
    Global,
    PerProject,
    PerProjectTagged,
}

/// Project label: lowercase basename (omp parity: git primary-checkout-root
/// basename, cwd basename outside a repo). Pure: the caller resolves the dir.
pub fn project_label(dir_basename: &str) -> String {
    dir_basename.trim().to_lowercase()
}

/// Bank for a scope: shared base for global/tagged, base + label for
/// per-project. Empty/absent labels never yield a trailing dash.
pub fn bank_for_scope(base: &HindsightConfig, mode: &Scoping, project: Option<&str>) -> String {
    if *mode != Scoping::PerProject {
        return base.bank.clone();
    }
    let label = project.map(project_label).unwrap_or_default();
    if label.is_empty() {
        return base.bank.clone();
    }
    format!("{}-{label}", base.bank)
}

/// Recall tag filter for a scope: tagged mode filters on the project label,
/// global/per-project read the bank itself (no tag filter). Pure.
pub fn tags_for_recall(mode: &Scoping, project: Option<&str>) -> Vec<String> {
    if *mode != Scoping::PerProjectTagged {
        return Vec::new();
    }
    let label = project.map(project_label).unwrap_or_default();
    if label.is_empty() {
        return Vec::new();
    }
    vec![format!("project:{label}")]
}

/// Global (untagged, every-project) writes need the explicit `globalScope`
/// flag AND a bank every project recalls (shared or tagged). Per-project
/// banks can never receive a global write — fail loud at the boundary.
pub fn global_retain_allowed(global_scope: bool, mode: &Scoping) -> bool {
    global_scope && *mode != Scoping::PerProject
}

/// Retain request paths (precedent-frozen; see `precedent.rs`).
pub fn retain_path(bank: &str) -> String {
    format!("/v1/default/banks/{bank}/memories")
}

/// Recall request path (precedent-frozen).
pub fn recall_path(bank: &str) -> String {
    format!("/v1/default/banks/{bank}/memories/recall")
}

/// Reflect request path (precedent-frozen; manual-only in B1).
pub fn reflect_path(bank: &str) -> String {
    format!("/v1/default/banks/{bank}/reflect")
}

/// Candidate cdylib-style locations live in the daemon (A3); this module
/// only shapes messages. Library search path hint for operators:
pub fn default_socket_hint() -> PathBuf {
    std::path::PathBuf::from("http://localhost:8888")
}

use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    /// A1: tag derivation merges defaults + scope, sorted + deduped.
    #[test]
    fn tags_merge_and_dedup() {
        let cfg = HindsightConfig {
            enabled: true,
            base_url: "http://x".into(),
            bank: "b".into(),
            default_tags: vec!["team".into(), "project:shop".into()],
            timeout_ms: 1500,
            max_queue: 8,
            scoping: Scoping::PerProjectTagged,
            global_scope: false,
            outbox_dir: None,
        };
        let tags = tags_for_scope(&cfg, Some("shop"), Some("ch1"));
        assert!(tags.contains(&"team".to_string()));
        assert!(tags.contains(&"project:shop".to_string()));
        assert!(tags.contains(&"channel:ch1".to_string()));
        let mut sorted = tags.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(tags, sorted, "tags must be sorted+deduped");
    }

    /// A1: paths match the frozen precedent.
    #[test]
    fn paths_match_precedent() {
        assert_eq!(retain_path("b"), "/v1/default/banks/b/memories");
        assert_eq!(recall_path("b"), "/v1/default/banks/b/memories/recall");
        assert_eq!(reflect_path("b"), "/v1/default/banks/b/reflect");
    }

    /// G1a RED: bank derivation per scoping mode (global / per-project /
    /// per-project-tagged). Fails until `bank_for_scope` exists.
    #[test]
    fn bank_derivation_follows_scoping_mode() {
        let base = HindsightConfig {
            bank: "team".to_string(),
            ..HindsightConfig::default()
        };
        assert_eq!(
            bank_for_scope(&base, &Scoping::Global, Some("Shop")),
            "team"
        );
        assert_eq!(
            bank_for_scope(&base, &Scoping::PerProject, Some("Shop")),
            "team-shop"
        );
        assert_eq!(
            bank_for_scope(&base, &Scoping::PerProjectTagged, Some("Shop")),
            "team"
        );
        // Empty/absent project never yields a trailing dash.
        assert_eq!(bank_for_scope(&base, &Scoping::PerProject, None), "team");
    }

    /// G1a RED: tag filter per scoping mode. Fails until `tags_for_recall` exists.
    #[test]
    fn recall_tags_follow_scoping_mode() {
        assert_eq!(
            tags_for_recall(&Scoping::Global, Some("shop")),
            Vec::<String>::new()
        );
        assert_eq!(
            tags_for_recall(&Scoping::PerProjectTagged, Some("Shop")),
            vec!["project:shop".to_string()]
        );
        assert_eq!(
            tags_for_recall(&Scoping::PerProjectTagged, None),
            Vec::<String>::new()
        );
    }

    /// G1a RED: global writes allowed only with globalScope + tagged/global
    /// mode. Fails until `global_retain_allowed` exists.
    #[test]
    fn global_retain_needs_flag_and_mode() {
        assert!(global_retain_allowed(true, &Scoping::PerProjectTagged));
        assert!(global_retain_allowed(true, &Scoping::Global));
        assert!(!global_retain_allowed(false, &Scoping::PerProjectTagged));
        assert!(!global_retain_allowed(true, &Scoping::PerProject));
        assert!(!global_retain_allowed(false, &Scoping::Global));
    }
}

/// Fuse Hindsight hits into a text section appended AFTER canonical results.
/// Pure: canonical text untouched, Hindsight hits deduplicated (repeat
/// document_ids skipped; snippets already present in the canonical text
/// skipped). Scores shown as-is, never fed into native ranking (protects
/// RQ-11 calibration).
pub fn merge_hindsight_hits(canonical_text: &str, hits: &[crate::hindsight::RecallHit]) -> String {
    use std::collections::HashSet;
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = String::new();
    let mut fresh = 0;
    for h in hits {
        if !seen.insert(h.document_id.as_str()) {
            continue;
        }
        let probe: String = h.snippet.chars().take(120).collect();
        if !probe.trim().is_empty() && canonical_text.contains(&probe) {
            continue;
        }
        fresh += 1;
        out.push_str(&format!(
            "  [{}] (score {:.2}) {}\n",
            h.document_id, h.score, h.snippet
        ));
    }
    if fresh == 0 {
        return String::new();
    }
    format!("Hindsight suggestions ({fresh} beyond canonical):\n{out}")
}

#[cfg(test)]
mod fusion_tests {
    use super::*;
    use crate::hindsight::RecallHit;

    fn hit(id: &str, snippet: &str) -> RecallHit {
        RecallHit {
            document_id: id.to_string(),
            score: 0.9,
            snippet: snippet.into(),
        }
    }

    /// A4: appended under a labeled section, repeats deduped.
    #[test]
    fn fusion_appends_and_dedups() {
        let hits = vec![hit("hs:1", "alpha snippet"), hit("hs:1", "alpha snippet")];
        let section = merge_hindsight_hits("canonical text", &hits);
        assert!(section.contains("Hindsight suggestions (1 beyond canonical)"));
        assert_eq!(section.matches("hs:1").count(), 1, "dedupes repeat hits");
    }

    /// A4: snippets already in the canonical text are skipped.
    #[test]
    fn skips_snippets_already_shown() {
        let hits = vec![hit("hs:1", "visible fact"), hit("hs:2", "novel fact")];
        let section = merge_hindsight_hits("results include visible fact here", &hits);
        assert!(!section.contains("hs:1"), "must skip shown snippet");
        assert!(section.contains("hs:2"), "must keep novel snippet");
    }

    /// A4: no fresh hits means empty section (no noise).
    #[test]
    fn empty_when_nothing_fresh() {
        assert_eq!(merge_hindsight_hits("t", &[]), "");
        assert_eq!(
            merge_hindsight_hits("visible fact", &[hit("a", "visible fact")]),
            ""
        );
    }
}
