//! Best-effort secret detection and redaction for `memory_add` (design 12.1).
//!
//! The default privacy path scans and redacts detected secrets before canonical
//! persistence. Detection is best-effort with testable patterns — it makes no
//! claim of perfect secret recognition. The `confirm=true` override stores
//! content verbatim (DEV-002: the approved legacy policy preserves the
//! override).

/// Detect whether a fragment contains a likely secret.
pub fn contains_secret(fragment: &str) -> bool {
    redact(fragment) != fragment
}

/// Redact likely secrets in a fragment, replacing values with a placeholder.
///
/// Best-effort: covers common API-key/token/password patterns. Returns the
/// input unchanged when nothing is detected.
pub fn redact(fragment: &str) -> String {
    let mut out = fragment.to_string();
    for pattern in secret_patterns() {
        out = pattern.replace(&out, "[REDACTED]").to_string();
    }
    out
}

/// Best-effort secret patterns (regex). Each replaces the whole match with a
/// placeholder. Kept conservative to avoid over-redacting legitimate content.
fn secret_patterns() -> Vec<regex::Regex> {
    [
        // api_key / apikey / api-key = "value" or : value
        r#"(?i)(api[_-]?key\s*[:=]\s*)["']?[A-Za-z0-9_-]{8,}["']?"#,
        // token / access_token / auth_token = "value"
        r#"(?i)((access|auth|secret|private)[_-]?token\s*[:=]\s*)["']?[A-Za-z0-9_-]{8,}["']?"#,
        // password / passwd = "value"
        r#"(?i)(password|passwd|pwd)\s*[:=]\s*["']?\S{4,}["']?"#,
        // AWS access key ID
        r"AKIA[0-9A-Z]{16}",
        // Generic long bearer tokens
        r"(?i)(bearer\s+)[A-Za-z0-9_.-]{20,}",
    ]
    .iter()
    .map(|s| regex::Regex::new(s).expect("secret pattern is a valid regex"))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_api_key() {
        assert!(contains_secret("api_key = sk_abc1234567890"));
    }

    #[test]
    fn detects_bearer() {
        assert!(contains_secret(
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc"
        ));
    }

    #[test]
    fn no_false_positive_on_clean_text() {
        assert!(!contains_secret("use tokio runtime for async work"));
    }

    #[test]
    fn redacts_secret_value() {
        let redacted = redact("api_key = sk_abc1234567890");
        assert!(redacted.contains("[REDACTED]"));
        assert!(!redacted.contains("sk_abc1234567890"));
    }
}
