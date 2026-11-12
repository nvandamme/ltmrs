//! Slice A A1: Hindsight bridge types + config.
//!
//! Opt-in, default off. No network in this module — pure types, ids, tags,
//! and env parsing. The async client (A2) is the only HTTP-touching code.

/// Bridge configuration (opt-in; `enabled=false` ignores everything else).
#[derive(Debug, Clone)]
pub struct HindsightConfig {
    pub enabled: bool,
    pub base_url: String,
    pub bank: String,
    pub default_tags: Vec<String>,
    pub timeout_ms: u64,
    pub max_queue: usize,
    /// Bank scoping (omp `hindsight.scoping` parity; default tagged).
    pub scoping: super::Scoping,
    /// Untagged every-project writes (omp `hindsight.globalScope`, default off).
    pub global_scope: bool,
    /// Durable outbox root (G1d; omp outbox parity). `None` = in-memory
    /// only (current behavior): failed retains drop with the process.
    /// Set = each retain persists to disk before the worker sends it, and
    /// failures stay for retry instead of dropping.
    pub outbox_dir: Option<std::path::PathBuf>,
}

impl Default for HindsightConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            bank: String::new(),
            default_tags: Vec::new(),
            timeout_ms: 1500,
            max_queue: 256,
            scoping: super::Scoping::PerProjectTagged,
            global_scope: false,
            outbox_dir: None,
        }
    }
}

/// CLI-first flag set (mirrors the frontend `HindsightFlags`; no env reads).
/// `url` + `bank` together enable the bridge; anything else alone is a
/// usage error (never a silently half-configured bridge).
#[derive(Debug, Clone, Default)]
pub struct HindsightCliFlags {
    pub url: Option<String>,
    pub bank: Option<String>,
    pub tags: Option<String>,
    pub timeout_ms: Option<u64>,
    pub scoping: Option<String>,
    pub global_scope: bool,
    pub outbox: Option<String>,
}

/// Build config from CLI flags (pure; no env, no I/O).
/// `None` (all-absent) = bridge disabled. Partial (url xor bank) errors loud.
pub fn hindsight_config_from_flags(
    f: &HindsightCliFlags,
) -> Result<Option<HindsightConfig>, String> {
    match (&f.url, &f.bank) {
        (None, None) => {
            if f.tags.is_some()
                || f.timeout_ms.is_some()
                || f.scoping.is_some()
                || f.global_scope
                || f.outbox.is_some()
            {
                return Err(
                    "hindsight options need --hindsight-url and --hindsight-bank together"
                        .to_string(),
                );
            }
            return Ok(None);
        }
        (Some(u), Some(b)) if !u.trim().is_empty() && !b.trim().is_empty() => {}
        _ => {
            return Err(
                "hindsight needs both --hindsight-url and --hindsight-bank (non-empty)".to_string(),
            );
        }
    }
    let url = f.url.clone().unwrap();
    let bank = f.bank.clone().unwrap();
    let default_tags = f
        .tags
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let scoping = match f.scoping.as_deref() {
        Some("global") => super::Scoping::Global,
        Some("per-project") => super::Scoping::PerProject,
        Some("per-project-tagged") | None => super::Scoping::PerProjectTagged,
        Some(other) => {
            return Err(format!(
                "bad --hindsight-scoping {other:?} (want global|per-project|per-project-tagged)"
            ));
        }
    };
    Ok(Some(HindsightConfig {
        enabled: true,
        base_url: url,
        bank,
        default_tags,
        timeout_ms: f.timeout_ms.unwrap_or(1500),
        max_queue: 256,
        scoping,
        global_scope: f.global_scope,
        outbox_dir: f
            .outbox
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(std::path::PathBuf::from),
    }))
}

/// A document to retain: stable id, tags, content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainDoc {
    pub document_id: String,
    pub bank: String,
    pub tags: Vec<String>,
    pub title: String,
    pub text: String,
    pub project: Option<String>,
}

/// A recall hit: id, score, snippet (scores shown as-is, never ranked natively).
#[derive(Debug, Clone)]
pub struct RecallHit {
    pub document_id: String,
    pub score: f64,
    pub snippet: String,
}

/// Deterministic document id: `ltmrs:{collection}:{entity_uuid}` where
/// collection ∈ `memory|guide|session`. Never legacy display ids.
pub fn document_id(collection: &str, id: &ltmrs_domain::id::EntityId) -> String {
    format!("ltmrs:{collection}:{id}", id = id.as_uuid())
}

/// Bridge error (typed; timeouts map to Unavailable, never to silent drops).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    Disabled,
    Unavailable(String),
    Rejected(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A1: document ids are stable and namespaced.
    #[test]
    fn document_id_stable_and_namespaced() {
        let id = ltmrs_domain::id::EntityId::new(uuid::Uuid::from_u128(7));
        let a = document_id("memory", &id);
        let b = document_id("memory", &id);
        assert_eq!(a, b);
        assert!(a.starts_with("ltmrs:memory:"));
        assert_ne!(a, document_id("guide", &id));
    }
}

#[cfg(test)]
mod flag_tests {
    use super::*;

    /// CLI-first: all-absent flags = disabled (None), never an error.
    #[test]
    fn all_absent_disables() {
        assert!(
            hindsight_config_from_flags(&HindsightCliFlags::default())
                .unwrap()
                .is_none()
        );
    }

    /// CLI-first: url+bank enables with defaults; partial errors loudly.
    #[test]
    fn url_bank_enable_partial_rejects() {
        let cfg = hindsight_config_from_flags(&HindsightCliFlags {
            url: Some("http://x".into()),
            bank: Some("team".into()),
            ..Default::default()
        })
        .unwrap()
        .unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.bank, "team");
        assert!(matches!(
            cfg.scoping,
            super::super::Scoping::PerProjectTagged
        ));
        let err = hindsight_config_from_flags(&HindsightCliFlags {
            url: Some("http://x".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.contains("both"));
    }

    /// CLI-first: scoping + globalScope + outbox parse.
    #[test]
    fn scoping_parses() {
        let cfg = hindsight_config_from_flags(&HindsightCliFlags {
            url: Some("http://x".into()),
            bank: Some("t".into()),
            scoping: Some("global".into()),
            global_scope: true,
            outbox: Some("/tmp/ob".into()),
            ..Default::default()
        })
        .unwrap()
        .unwrap();
        assert!(matches!(cfg.scoping, super::super::Scoping::Global));
        assert!(cfg.global_scope);
        assert_eq!(cfg.outbox_dir, Some(std::path::PathBuf::from("/tmp/ob")));
        assert!(
            hindsight_config_from_flags(&HindsightCliFlags {
                url: Some("http://x".into()),
                bank: Some("t".into()),
                scoping: Some("nonsense".into()),
                ..Default::default()
            })
            .is_err()
        );
    }
}
