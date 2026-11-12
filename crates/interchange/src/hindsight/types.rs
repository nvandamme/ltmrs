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

/// Parse config from `LTMRS_HINDSIGHT_*` env (explicit wins, garbage fails loud).
pub fn hindsight_config_from_env() -> Result<HindsightConfig, String> {
    let enabled = match std::env::var("LTMRS_HINDSIGHT_ENABLED").as_deref() {
        Ok("1") | Ok("true") => true,
        Ok("0") | Ok("false") => false,
        Ok(other) => {
            return Err(format!(
                "bad LTMRS_HINDSIGHT_ENABLED={other:?} (want 0/1/true/false)"
            ));
        }
        Err(_) => false,
    };
    if !enabled {
        return Ok(HindsightConfig::default());
    }
    let base_url = std::env::var("LTMRS_HINDSIGHT_URL")
        .map_err(|_| "LTMRS_HINDSIGHT_URL must be set when the bridge is enabled".to_string())?;
    let bank = std::env::var("LTMRS_HINDSIGHT_BANK")
        .map_err(|_| "LTMRS_HINDSIGHT_BANK must be set when the bridge is enabled".to_string())?;
    if base_url.trim().is_empty() || bank.trim().is_empty() {
        return Err("LTMRS_HINDSIGHT_URL/BANK must be non-empty".to_string());
    }
    let default_tags = std::env::var("LTMRS_HINDSIGHT_TAGS")
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let timeout_ms = match std::env::var("LTMRS_HINDSIGHT_TIMEOUT_MS") {
        Ok(s) => s
            .parse::<u64>()
            .map_err(|_| format!("bad LTMRS_HINDSIGHT_TIMEOUT_MS={s:?}"))?,
        Err(_) => 1500,
    };
    let scoping = match std::env::var("LTMRS_HINDSIGHT_SCOPING").as_deref() {
        Ok("global") => super::Scoping::Global,
        Ok("per-project") => super::Scoping::PerProject,
        Ok("per-project-tagged") | Err(_) => super::Scoping::PerProjectTagged,
        Ok(other) => {
            return Err(format!(
                "bad LTMRS_HINDSIGHT_SCOPING={other:?} (want global|per-project|per-project-tagged)"
            ));
        }
    };
    let global_scope = match std::env::var("LTMRS_HINDSIGHT_GLOBAL_SCOPE").as_deref() {
        Ok("1") | Ok("true") => true,
        Ok("0") | Ok("false") | Err(_) => false,
        Ok(other) => {
            return Err(format!(
                "bad LTMRS_HINDSIGHT_GLOBAL_SCOPE={other:?} (want 0/1/true/false)"
            ));
        }
    };
    let outbox_dir = match std::env::var("LTMRS_HINDSIGHT_OUTBOX").as_deref() {
        Ok(s) if !s.trim().is_empty() => Some(std::path::PathBuf::from(s)),
        Ok(_) | Err(_) => None,
    };
    Ok(HindsightConfig {
        enabled,
        base_url,
        bank,
        default_tags,
        timeout_ms,
        max_queue: 256,
        scoping,
        global_scope,
        outbox_dir,
    })
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
    use std::sync::Mutex;

    /// Env-var tests mutate process-global state: serialize them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env_lock(f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Scrub bridge vars so tests never inherit ambient config.
        for k in [
            "LTMRS_HINDSIGHT_ENABLED",
            "LTMRS_HINDSIGHT_URL",
            "LTMRS_HINDSIGHT_BANK",
            "LTMRS_HINDSIGHT_TAGS",
            "LTMRS_HINDSIGHT_TIMEOUT_MS",
        ] {
            unsafe { std::env::remove_var(k) };
        }
        f();
        for k in [
            "LTMRS_HINDSIGHT_ENABLED",
            "LTMRS_HINDSIGHT_URL",
            "LTMRS_HINDSIGHT_BANK",
            "LTMRS_HINDSIGHT_TAGS",
            "LTMRS_HINDSIGHT_TIMEOUT_MS",
        ] {
            unsafe { std::env::remove_var(k) };
        }
    }

    /// A1 RED: disabled by default (no env).
    #[test]
    fn disabled_by_default() {
        let cfg = HindsightConfig::default();
        assert!(!cfg.enabled);
    }

    /// A1: env parsing enables with required URL + bank.
    #[test]
    fn env_parsing_requires_url_and_bank() {
        with_env_lock(|| {
            unsafe {
                std::env::set_var("LTMRS_HINDSIGHT_ENABLED", "1");
                std::env::set_var("LTMRS_HINDSIGHT_URL", "http://127.0.0.1:8888");
                std::env::set_var("LTMRS_HINDSIGHT_BANK", "team");
                std::env::remove_var("LTMRS_HINDSIGHT_TAGS");
                std::env::remove_var("LTMRS_HINDSIGHT_TIMEOUT_MS");
            }
            let cfg = hindsight_config_from_env().unwrap();
            assert!(cfg.enabled);
            assert_eq!(cfg.bank, "team");
            assert_eq!(cfg.timeout_ms, 1500);
            unsafe {
                std::env::remove_var("LTMRS_HINDSIGHT_ENABLED");
                std::env::remove_var("LTMRS_HINDSIGHT_URL");
                std::env::remove_var("LTMRS_HINDSIGHT_BANK");
            }
        });
    }

    /// A1: garbage enabled flag fails loud.
    #[test]
    fn garbage_enabled_fails_loud() {
        with_env_lock(|| {
            unsafe { std::env::set_var("LTMRS_HINDSIGHT_ENABLED", "maybe") };
            assert!(hindsight_config_from_env().is_err());
        });
    }

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
