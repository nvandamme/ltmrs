//! Text helpers shared by tool adapters (moved verbatim from `tools.rs`):
//! auto-titles, auto-descriptions and canonical project keys.

/// Canonical project key: trimmed + lowercased, path collapsed to basename.
/// "global" (case-insensitive) maps to None.
pub(crate) fn normalize_project(raw: &str) -> Option<String> {
    ltmrs_compat::lemma::tool_args::normalize_project(raw)
}

/// Auto-title: first 40 chars (truncated with "...") or the fragment itself.
/// Char-boundary truncation: byte slicing would panic on multi-byte titles.
/// Compat nuance: upstream JS substring counts UTF-16 units, so astral-plane
/// chars (emoji/CJK-ext) count 1 here vs 2 there — strictly better than
/// panicking, and titles are display hints, not protocol.
pub(crate) fn generate_title(fragment: &str) -> String {
    if fragment.chars().count() > 40 {
        format!("{}...", fragment.chars().take(40).collect::<String>())
    } else {
        fragment.to_string()
    }
}

/// Auto-description: first sentence if short, else first 80 chars.
pub(crate) fn generate_description(fragment: &str) -> String {
    // Upstream (JavaScript) measures length and slices in UTF-16 code units,
    // not bytes or Unicode code points. Replicate that exactly.
    let units: Vec<u16> = fragment.encode_utf16().collect();
    if units.len() <= 80 {
        return fragment.to_string();
    }
    let first = fragment.split(['.', '!', '?', '\n']).next().unwrap_or("");
    let first_units = first.encode_utf16().count();
    if !first.is_empty() && first_units <= 100 {
        // `first` is everything before the first delimiter, so it never ends
        // with '.' — upstream always appends '...'.
        return format!("{}...", first.trim());
    }
    // Upstream: fragment.substring(0, 80).trim() + '...'
    // from_utf16_lossy handles surrogate pairs correctly and maps a lone
    // surrogate (from cutting mid-emoji) to U+FFFD, matching JS behavior.
    let truncated = String::from_utf16_lossy(&units[..80]);
    format!("{}...", truncated.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Auto-title truncates by characters, never by bytes (multi-byte input
    /// must not panic at the boundary).
    #[test]
    fn generate_title_truncates_by_chars() {
        assert_eq!(generate_title("short"), "short");
        let long_ascii = "a".repeat(41);
        assert_eq!(
            generate_title(&long_ascii),
            format!("{}...", "a".repeat(40))
        );
        // Emoji past the char boundary: byte slicing would panic.
        let emoji = "😀".repeat(41);
        let titled = generate_title(&emoji);
        assert_eq!(titled.chars().count(), 43, "40 chars + ellipsis");
        assert!(titled.ends_with("..."));
        // Byte-boundary only (20 emoji = 20 chars): no truncation, no panic.
        let short_emoji = "😀".repeat(20);
        assert!(short_emoji.len() > 40, "fixture crosses the byte boundary");
        assert_eq!(generate_title(&short_emoji), short_emoji);
    }
}
