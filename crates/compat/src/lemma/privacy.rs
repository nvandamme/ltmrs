//! Best-effort secret detection and redaction for `memory_add` (design 12.1).
//!
//! The default privacy path scans and redacts detected secrets before canonical
//! persistence. Detection is best-effort with testable patterns — it makes no
//! claim of perfect secret recognition. The `confirm=true` override stores
//! non-critical content verbatim, but critical secrets (irrevocable
//! credentials) are always redacted (DEV-002).

/// Detect whether a fragment contains a likely secret.
pub fn contains_secret(fragment: &str) -> bool {
    redact(fragment) != fragment
}

/// Detect whether a fragment contains a critical secret that no override can
/// store verbatim.
pub fn contains_critical_secret(fragment: &str) -> bool {
    critical_patterns()
        .iter()
        .any(|pattern| pattern.is_match(fragment))
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
        // GitHub PATs: ghp_ / gho_ / github_pat_ prefixes
        r"ghp_[A-Za-z0-9]{20,}",
        r"gho_[A-Za-z0-9]{20,}",
        r"github_pat_[A-Za-z0-9_]{20,}",
        // PEM private-key block header
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----",
        // Google API keys
        r"AIza[0-9A-Za-z_-]{35}",
        // Slack tokens
        r"xox[bpas]-[A-Za-z0-9-]{10,}",
        // Webhook URLs carrying secrets
        r"discord\.com/api/webhooks/[A-Za-z0-9/_-]+",
        r"hooks\.slack\.com/[A-Za-z0-9/_-]+",
        // Database connection strings with embedded credentials
        r"(postgres|postgresql|mysql|mongodb)(\+srv)?://[^/\s]*:[^/\s]*@[^/\s]+",
    ]
    .iter()
    .map(|s| regex::Regex::new(s).expect("secret pattern is a valid regex"))
    .collect()
}

/// Critical secret patterns: high-confidence irrevocable credentials that no
/// `confirm=true` override may store verbatim.
fn critical_patterns() -> Vec<regex::Regex> {
    [
        r"AKIA[0-9A-Z]{16}",
        r"ghp_[A-Za-z0-9]{20,}",
        r"gho_[A-Za-z0-9]{20,}",
        r"github_pat_[A-Za-z0-9_]{20,}",
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----",
    ]
    .iter()
    .map(|s| regex::Regex::new(s).expect("critical pattern is a valid regex"))
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

    #[test]
    fn detects_github_pat_ghp() {
        assert!(contains_secret(
            "token ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD"
        ));
    }

    #[test]
    fn detects_github_pat_gho() {
        assert!(contains_secret(
            "token gho_abcdefghijklmnopqrstuvwxyz0123456789ABCD"
        ));
    }

    #[test]
    fn detects_github_pat_prefix() {
        assert!(contains_secret(
            "token github_pat_abcdefghijklmnopqrstuvwxyz0123456789ABCD"
        ));
    }

    #[test]
    fn no_false_positive_on_short_ghp() {
        assert!(!contains_secret("ghp_short"));
    }

    #[test]
    fn detects_pem_private_key() {
        assert!(contains_secret(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA7b..."
        ));
    }

    #[test]
    fn no_false_positive_on_certificate_header() {
        assert!(!contains_secret("-----BEGIN CERTIFICATE-----\nMIID..."));
    }

    #[test]
    fn detects_google_api_key() {
        assert!(contains_secret(
            "key AIzaSyC12345678901234567890123456789012 end"
        ));
    }

    #[test]
    fn no_false_positive_on_short_aiza() {
        assert!(!contains_secret("key AIzaShort end"));
    }

    #[test]
    fn detects_slack_token() {
        assert!(contains_secret("token xoxb-123456789012-ABCDEFGHIJKLmnop"));
    }

    #[test]
    fn no_false_positive_on_short_slack_token() {
        assert!(!contains_secret("token xoxb-abc end"));
    }

    #[test]
    fn detects_discord_webhook() {
        assert!(contains_secret(
            "hook https://discord.com/api/webhooks/1234567890/abcdefghijklmnopqrstuvwxyz_secret-token-123"
        ));
    }

    #[test]
    fn detects_slack_webhook() {
        assert!(contains_secret(
            "hook https://hooks.slack.com/services/T123456/B789012/xyz789secretTOKEN123abc"
        ));
    }

    #[test]
    fn no_false_positive_on_generic_webhook_path() {
        assert!(!contains_secret(
            "see https://example.com/api/webhooks/123 for docs"
        ));
    }

    #[test]
    fn detects_postgres_connection_string() {
        assert!(contains_secret(
            "db postgres://alice:s3cr3t@db.example.com:5432/mydb end"
        ));
    }

    #[test]
    fn detects_mysql_connection_string() {
        assert!(contains_secret(
            "db mysql://bob:p4ssw0rd@db.example.com/mydb end"
        ));
    }

    #[test]
    fn detects_mongodb_connection_string() {
        assert!(contains_secret(
            "db mongodb://carol:hunter2@mongo.example.com:27017/admin end"
        ));
    }

    #[test]
    fn no_false_positive_on_credentialless_connection_string() {
        assert!(!contains_secret("db postgres://db.example.com/mydb end"));
    }

    #[test]
    fn critical_flags_pem_block() {
        assert!(contains_critical_secret(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA7b..."
        ));
    }

    #[test]
    fn critical_flags_github_pat() {
        assert!(contains_critical_secret(
            "token ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD"
        ));
    }

    #[test]
    fn critical_flags_aws_key() {
        assert!(contains_critical_secret("key AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn critical_ignores_non_critical_secret() {
        assert!(!contains_critical_secret("api_key = sk_abc1234567890"));
    }

    #[test]
    fn critical_ignores_clean_text() {
        assert!(!contains_critical_secret(
            "use tokio runtime for async work"
        ));
    }

    #[test]
    fn critical_samples_are_always_detected_as_secrets() {
        for sample in [
            "key AKIAIOSFODNN7EXAMPLE",
            "token ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD",
            "token gho_abcdefghijklmnopqrstuvwxyz0123456789ABCD",
            "token github_pat_abcdefghijklmnopqrstuvwxyz0123456789ABCD",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA7b...",
            "-----BEGIN PRIVATE KEY-----\nMIIE...",
        ] {
            assert!(contains_secret(sample), "critical sample missed: {sample}");
            assert!(contains_critical_secret(sample));
        }
    }
}
