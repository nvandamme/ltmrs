//! Lemma-faithful TF-IDF retrieval profile (Wave B5b).
//!
//! Reference port of upstream Lemma 0.21.0 `src/intelligence/semantic.ts`
//! (pinned baseline commit `d30a816`; verified against upstream `main`):
//! corpus-wide TF-IDF cosine similarity with an explicit similarity
//! threshold, served as the `hybrid: false` path so the flag is truthful.
//!
//! Reference behavior, ported verbatim where noted:
//! - Tokenizer: lowercase, non-`[a-z0-9\s-]` to space, split on whitespace,
//!   keep `len > 2` and outside the upstream stopword list.
//! - Term frequency: augmented `0.5 + 0.5 * freq / maxFreq` (per document,
//!   query included).
//! - IDF: `ln((n + 1) / (df + 1)) + 1` over the served corpus (smoothed, so
//!   ubiquitous terms weigh 1.0 instead of zeroing single-document corpora).
//! - Query weighting uses the corpus df table and corpus size — never a
//!   single-document scale (upstream warns this flips common-term weights).
//! - Threshold: `score >= 0.15` (upstream `findSemanticSimilar` default);
//!   below-threshold results are excluded and an empty ranking is the
//!   caller's abstention signal.
//! - Ordering: score descending. Upstream relies on stable row order for
//!   ties; the deterministic equivalent here is EntityId ascending.
//!
//! The query path (`rank`) is [`rank`]; the corpus text rendering is
//! [`corpus_text`]. Both are pure and boundary-tested below.

use std::collections::{HashMap, HashSet};

use ltmrs_domain::id::EntityId;

/// Explicit similarity floor (upstream `findSemanticSimilar` default):
/// only scores at or above this are served.
pub const TFIDF_THRESHOLD: f64 = 0.15;

/// Upstream stopword list (`semantic.ts` STOP_WORDS), ported verbatim. This
/// is the semantic-retrieval list, not the conflict detector's overlap list.
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "could", "should", "may", "might", "can", "shall", "to",
    "of", "in", "for", "on", "with", "at", "by", "from", "as", "into", "through", "during",
    "before", "after", "above", "below", "between", "out", "off", "over", "under", "again",
    "further", "then", "once", "here", "there", "when", "where", "why", "how", "all", "each",
    "every", "both", "few", "more", "most", "other", "some", "such", "no", "nor", "not", "only",
    "own", "same", "so", "than", "too", "very", "just", "because", "but", "and", "or", "if",
    "while", "about", "up", "it", "its", "this", "that", "these", "those", "i", "me", "my", "we",
    "our", "you", "your", "he", "him", "his", "she", "her", "they", "them", "their", "what",
    "which", "who", "whom", "am", "also", "use", "used", "using", "make", "made", "like", "get",
    "got", "one", "two", "new", "first", "last", "long", "great", "little", "just", "know", "time",
    "way", "well", "back", "even", "still", "much", "many", "need", "going", "thing", "think",
];

/// Upstream `tokenize`: lowercase, non-`[a-z0-9\s-]` runs to space, split on
/// whitespace, drop `len <= 2` and stopwords. Hyphenated compounds stay one
/// token; non-ASCII letters are separators, exactly as upstream.
pub fn tokenize(text: &str) -> Vec<String> {
    let cleaned: String = text
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    cleaned
        .split_whitespace()
        .filter(|w| w.len() > 2 && !STOP_WORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Corpus text for one memory (upstream `buildVectors` interpolation):
/// title, fragment and description, space-joined.
pub fn corpus_text(title: &str, fragment: &str, description: &str) -> String {
    format!("{title} {fragment} {description}")
}

/// Upstream `computeTermFrequency`: augmented `0.5 + 0.5 * freq / maxFreq`.
fn term_frequency(tokens: &[String]) -> HashMap<String, f64> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for token in tokens {
        *counts.entry(token).or_insert(0) += 1;
    }
    let max_freq = counts.values().max().copied().unwrap_or(1).max(1) as f64;
    counts
        .into_iter()
        .map(|(term, freq)| (term.to_string(), 0.5 + 0.5 * freq as f64 / max_freq))
        .collect()
}

/// Upstream `computeDocumentFrequency` + `applyIdf` weight in one pass:
/// `w = tf * (ln((n + 1) / (df + 1)) + 1)`, plus the vector norm. Terms are
/// counted once per document (df), never per occurrence.
fn weigh(
    tf: &HashMap<String, f64>,
    df: &HashMap<String, usize>,
    n: usize,
) -> (HashMap<String, f64>, f64) {
    let mut weighted: HashMap<String, f64> = HashMap::with_capacity(tf.len());
    let mut sum_sq = 0.0;
    for (term, value) in tf {
        let idf = ((n as f64 + 1.0) / (df.get(term.as_str()).copied().unwrap_or(0) as f64 + 1.0))
            .ln()
            + 1.0;
        let w = value * idf;
        sum_sq += w * w;
        weighted.insert(term.clone(), w);
    }
    (weighted, sum_sq.sqrt())
}

/// Upstream `cosineSimilarity`: zero-norm vectors score 0.0 (never NaN).
fn cosine(a: &(HashMap<String, f64>, f64), b: &(HashMap<String, f64>, f64)) -> f64 {
    if a.1 == 0.0 || b.1 == 0.0 {
        return 0.0;
    }
    let mut dot = 0.0;
    for (term, val) in &a.0 {
        if let Some(b_val) = b.0.get(term) {
            dot += val * b_val;
        }
    }
    dot / (a.1 * b.1)
}

/// Rank a corpus snapshot for a query: upstream TF-IDF cosine with the
/// explicit threshold, score-descending with EntityId-ascending tiebreak.
/// Returns the served set only; an empty result is the caller's abstention
/// signal (mirrors upstream returning `[]`).
pub fn rank(query: &str, docs: &[(EntityId, String)]) -> Vec<(EntityId, f64)> {
    let query_tokens = tokenize(query);
    if query_tokens.is_empty() || docs.is_empty() {
        return Vec::new();
    }
    let docs_tokens: Vec<Vec<String>> = docs.iter().map(|(_, text)| tokenize(text)).collect();
    let mut df: HashMap<String, usize> = HashMap::new();
    for tokens in &docs_tokens {
        for term in tokens.iter().collect::<HashSet<_>>() {
            *df.entry(term.clone()).or_insert(0) += 1;
        }
    }
    let n = docs.len();
    let query_vec = weigh(&term_frequency(&query_tokens), &df, n);
    let mut scored: Vec<(EntityId, f64)> = docs
        .iter()
        .zip(docs_tokens.iter())
        .map(|((id, _), tokens)| {
            (
                *id,
                cosine(&query_vec, &weigh(&term_frequency(tokens), &df, n)),
            )
        })
        .filter(|(_, score)| score.is_finite() && *score >= TFIDF_THRESHOLD)
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored
}

#[cfg(test)]
mod tfidf_profile_tests {
    use uuid::Uuid;

    use ltmrs_domain::id::EntityId;

    use super::{corpus_text, rank, tokenize};

    fn eid(n: u128) -> EntityId {
        EntityId::new(Uuid::from_u128(n))
    }

    /// Toy corpus ranking: upstream-formula TF-IDF cosine order.
    ///
    /// Corpus (N = 3):
    ///   D1 = "alpha beta gamma"   -> tf 1.0 each (maxF 1)
    ///   D2 = "alpha alpha delta"   -> alpha 1.0 (maxF 2), delta 0.75
    ///   D3 = "epsilon zeta eta"    -> tf 1.0 each
    /// Query = "alpha beta" -> tf 1.0 each.
    ///
    /// Hand arithmetic (idf = ln((N+1)/(df+1)) + 1; df: alpha -> 2,
    /// beta/gamma/delta -> 1):
    ///   idf(alpha) = ln(4/3) + 1 = 1.2876820725,
    ///   idf(beta) = idf(gamma) = idf(delta) = ln(2) + 1 = 1.6931471806.
    ///   q = (1.2876820725, 1.6931471806),
    ///     |q|^2 = 1.6581200538 + 2.8667506049 = 4.5248706587.
    ///   D1: dot = |q|^2 = 4.5248706587; |D1|^2 adds gamma's 2.8667506049
    ///     = 7.3916212636; cosine = 4.5248706587 / (|q| * sqrt(7.3916212636))
    ///     = 0.7824081412 (machine-checked).
    ///   D2: dot = 1.2876820725^2 = 1.6581200538;
    ///     |D2|^2 = 1.6581200538 + (0.75 * 1.6931471806)^2
    ///     = 1.6581200538 + 1.6127977890 = 3.2709178428;
    ///     cosine = 1.6581200538 / (|q| * sqrt(3.2709178428))
    ///     = 0.4310183179 (machine-checked).
    ///   D3 shares no term -> 0.0, below the threshold, excluded.
    ///
    /// Expected: [D1 ~= 0.7824081412, D2 ~= 0.4310183179], D3 absent.
    #[test]
    fn ranks_toy_corpus_in_expected_order() {
        let docs = vec![
            (eid(1), "alpha beta gamma".to_string()),
            (eid(2), "alpha alpha delta".to_string()),
            (eid(3), "epsilon zeta eta".to_string()),
        ];
        let ranked = rank("alpha beta", &docs);
        assert_eq!(ranked.len(), 2, "D3 must be excluded, D1/D2 kept");
        assert_eq!(ranked[0].0, eid(1));
        assert!(
            (ranked[0].1 - 0.782_408_141_2).abs() < 1e-9,
            "D1 hand-computed cosine: {}",
            ranked[0].1
        );
        assert_eq!(ranked[1].0, eid(2));
        assert!(
            (ranked[1].1 - 0.431_018_317_9).abs() < 1e-9,
            "D2 hand-computed cosine: {}",
            ranked[1].1
        );
    }

    /// Abstention: a query with no shared term scores 0.0 everywhere, below
    /// the 0.15 floor, so the ranked set is empty.
    #[test]
    fn threshold_abstains_when_no_term_overlap() {
        let docs = vec![
            (eid(1), "alpha beta gamma".to_string()),
            (eid(2), "alpha alpha delta".to_string()),
        ];
        assert!(
            rank("qqq zzz", &docs).is_empty(),
            "zero overlap must abstain, not serve noise"
        );
    }

    /// Upstream tokenizer rules: `len <= 2` and stopwords are dropped, so a
    /// query of only such tokens tokenizes empty and abstains.
    #[test]
    fn short_tokens_and_stopwords_are_ignored() {
        assert!(tokenize("a I x to be or").is_empty());
        assert_eq!(
            tokenize("Rust cargo-testing!"),
            vec!["rust", "cargo-testing"]
        );
        let docs = vec![(eid(1), "alpha beta".to_string())];
        assert!(rank("a", &docs).is_empty());
    }

    /// Tied scores keep a deterministic order: EntityId ascending (the
    /// deterministic equivalent of upstream's stable row order). The third
    /// document keeps both twins at cosine exactly 1.0 on parallel vectors.
    #[test]
    fn ties_break_by_entity_id() {
        let docs = vec![
            (eid(9), "alpha beta".to_string()),
            (eid(4), "alpha beta".to_string()),
            (eid(7), "gamma delta epsilon".to_string()),
        ];
        let ranked = rank("alpha beta", &docs);
        assert_eq!(ranked.len(), 2);
        assert!((ranked[0].1 - 1.0).abs() < 1e-12);
        assert!((ranked[1].1 - 1.0).abs() < 1e-12);
        assert_eq!(ranked[0].0, eid(4));
        assert_eq!(ranked[1].0, eid(9));
    }

    /// The threshold is explicit (0.15): only scores at or above it are
    /// served. Single shared term over a two-document corpus scores exactly
    /// 1/sqrt(2) ~= 0.7071067812 (hand: equal idf cancels, cosine of a
    /// one-hot query against a two-term document).
    #[test]
    fn threshold_is_explicit() {
        let docs = vec![
            (eid(1), "alpha beta".to_string()),
            (eid(2), "gamma delta".to_string()),
        ];
        let ranked = rank("alpha", &docs);
        assert_eq!(ranked.len(), 1);
        assert!(
            (ranked[0].1 - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-9,
            "single-term cosine: {}",
            ranked[0].1
        );
    }

    /// Corpus text joins title, fragment and description with spaces,
    /// mirroring the upstream vector-build interpolation.
    #[test]
    fn corpus_text_joins_all_fields() {
        assert_eq!(corpus_text("T", "F", "D"), "T F D".to_string());
    }
}
