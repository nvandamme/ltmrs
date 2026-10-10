//! E5-small adapter tests (moved verbatim from `e5_small.rs`).

use std::path::PathBuf;

use super::{E5SmallAdapter, EmbedInput};
use crate::artifacts::{ArtifactCache, ArtifactError};
use crate::manifest::{e5_small_artifact, e5_small_recipe};
use crate::recipe::Role;

/// Load the reference fixture JSON from the tracked fixtures directory.
fn load_fixture() -> serde_json::Value {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/fixtures/reference_fixture.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Populate the cache directory with the pinned artifact files so tests can
/// run offline against real weights. Returns false (skip) when artifacts are
/// not present in tmp/e5-artifacts (CI without network). The artifacts live
/// at the workspace root, two levels above this crate's manifest dir.
fn populate_cache(cache: &ArtifactCache, _guard: &tempfile::TempDir) -> bool {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tmp/e5-artifacts");
    if !src.join("model.safetensors").exists() {
        return false;
    }
    let artifact = e5_small_artifact();
    let mdir = cache.model_dir(&artifact);
    std::fs::create_dir_all(&mdir).unwrap();
    for file in artifact.digests.keys() {
        let from = src.join(file);
        if from.exists() {
            std::fs::copy(&from, mdir.join(file)).unwrap();
        }
    }
    true
}

fn test_cache() -> (ArtifactCache, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (ArtifactCache::new(dir.path()), dir)
}

/// T-EMB-01: tokenizer IDs and special tokens match the pinned reference.

#[test]
fn t_emb_01_tokenizer_matches_reference() {
    let (cache, guard) = test_cache();
    if !populate_cache(&cache, &guard) {
        eprintln!("SKIP: artifacts not present");
        return;
    }
    let adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();
    let fixture = load_fixture();

    // Reference fixture special tokens (from tokenizer.json at pin time):
    // pad=1, eos/sep=2, cls/bos=0.
    assert_eq!(adapter.special_token_ids().0, 1, "pad token id");
    assert_eq!(adapter.special_token_ids().1, 2, "eos token id");

    // short_en case: query role on "How to configure Fjall persistence mode?".
    let ref_q: Vec<u32> = fixture["cases"]["short_en"]["input_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_u64())
        .map(|v| v as u32)
        .collect();
    let q_tokenized = adapter.tokenize("How to configure Fjall persistence mode?", Role::Query);
    assert_eq!(
        q_tokenized, ref_q,
        "query tokenization must match reference"
    );

    // FR query case (non-English).
    let ref_fr: Vec<u32> = fixture["cases"]["fr_query"]["input_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_u64())
        .map(|v| v as u32)
        .collect();
    let fr_tokenized = adapter.tokenize("comment configurer la persistance ?", Role::Query);
    assert_eq!(
        fr_tokenized, ref_fr,
        "FR query tokenization must match reference"
    );

    // Code snippet case (passage role).
    let ref_code: Vec<u32> = fixture["cases"]["code_snippet"]["input_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_u64())
        .map(|v| v as u32)
        .collect();
    let code_tokenized = adapter.tokenize("fn main() { println!(\"hello\"); }", Role::Passage);
    assert_eq!(
        code_tokenized, ref_code,
        "code snippet tokenization must match reference"
    );

    // Prefix asymmetry: query vs passage on same text produce different IDs.
    let q_ids = adapter.tokenize("How to configure Fjall persistence mode?", Role::Query);
    let p_ids = adapter.tokenize("How to configure Fjall persistence mode?", Role::Passage);
    assert_ne!(
        q_ids, p_ids,
        "query and passage prefixes must produce different token sequences"
    );

    // Prefix IDs match reference fixture.
    let ref_p: Vec<u32> = fixture["prefix"]["passage_input_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_u64())
        .map(|v| v as u32)
        .collect();
    assert_eq!(
        p_ids, ref_p,
        "passage prefix tokenization must match reference"
    );

    // Model window is 512.
    assert_eq!(adapter.recipe().max_tokens, 512);
}

/// T-EMB-02: single and batched embeddings on empty/short/padded inputs;
/// shapes, finiteness, normalization and reference equivalence.

#[test]
fn t_emb_02_embeddings_match_reference() {
    let (cache, guard) = test_cache();
    if !populate_cache(&cache, &guard) {
        eprintln!("SKIP: artifacts not present");
        return;
    }
    let mut adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();
    let fixture = load_fixture();

    // Tolerance rationale (see manifest): CPU F32 Candle vs PyTorch F32.
    const COSINE_TOL: f32 = 0.999;
    const MAX_ABS_DIFF: f32 = 1e-3;

    fn check_vector(actual: &[f32], expected_json: &serde_json::Value, label: &str) {
        let expected: Vec<f32> = expected_json
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_f64())
            .map(|v| v as f32)
            .collect();
        assert_eq!(actual.len(), expected.len(), "{label}: dimension mismatch");

        // Finiteness.
        assert!(
            actual.iter().all(|v| v.is_finite()),
            "{label}: non-finite values"
        );

        // L2 normalization: norm should be ~1.0.
        let norm_sq: f32 = actual.iter().map(|v| v * v).sum();
        assert!(
            (norm_sq - 1.0).abs() < 1e-4,
            "{label}: not unit-normalized (norm^2={norm_sq})"
        );

        // Cosine similarity (both normalized, so dot product = cosine).
        let cos: f32 = actual.iter().zip(expected.iter()).map(|(a, b)| a * b).sum();
        assert!(
            cos > COSINE_TOL,
            "{label}: cosine {cos} below tolerance {COSINE_TOL}"
        );

        // Max absolute difference.
        let max_diff: f32 = actual
            .iter()
            .zip(expected.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(
            max_diff < MAX_ABS_DIFF,
            "{label}: max abs diff {max_diff} above {MAX_ABS_DIFF}"
        );

        eprintln!("{label}: OK (cos={cos:.6}, max_diff={max_diff:.8e})");
    }

    // short_en: query role.
    let seq = adapter
        .embed("How to configure Fjall persistence mode?", Role::Query)
        .unwrap();
    check_vector(
        &seq.vector,
        &fixture["cases"]["short_en"]["vector"],
        "short_en/query",
    );

    // Query vs passage asymmetry on same text.
    let q_seq = adapter
        .embed("How to configure Fjall persistence mode?", Role::Query)
        .unwrap();
    let p_seq = adapter
        .embed("How to configure Fjall persistence mode?", Role::Passage)
        .unwrap();
    check_vector(
        &p_seq.vector,
        &fixture["prefix"]["passage_vector"],
        "short_en/passage",
    );

    // The two roles must produce meaningfully different vectors.
    let cos_q_p: f32 = q_seq
        .vector
        .iter()
        .zip(p_seq.vector.iter())
        .map(|(a, b)| a * b)
        .sum();
    assert!(
        cos_q_p < 0.995,
        "query and passage embeddings should differ (cos={cos_q_p})"
    );

    // mixed_batch: padded batch of two different-length inputs (passage role).
    let short = "short one";
    let long =
        "a somewhat longer sentence about Rust memory models and ownership rules in the compiler";
    let results = adapter
        .embed_batch(&[
            EmbedInput {
                text: short.into(),
                role: Role::Passage,
            },
            EmbedInput {
                text: long.into(),
                role: Role::Passage,
            },
        ])
        .unwrap();
    assert_eq!(results.len(), 2);

    // Reference was generated with the same inputs (passage prefix included).
    check_vector(
        &results[0].vector,
        &fixture["cases"]["mixed_batch"]["vectors"][0],
        "mixed_batch[0]",
    );
    check_vector(
        &results[1].vector,
        &fixture["cases"]["mixed_batch"]["vectors"][1],
        "mixed_batch[1]",
    );

    // Batch row 0 must equal single embed (padding correctness).
    let r0 = adapter.embed(short, Role::Passage).unwrap();
    assert_eq!(
        results[0].vector, r0.vector,
        "batch row 0 must equal single embed"
    );

    // Padding correctness: batch attention masks reflect actual lengths.
    let len0 = results[0].attention_mask.iter().filter(|&&b| b).count();
    let len1 = results[1].attention_mask.iter().filter(|&&b| b).count();
    assert!(len1 > len0, "longer text must have more attended tokens");

    // empty_string case (passage role on empty string).
    let empty_seq = adapter.embed("", Role::Passage).unwrap();
    check_vector(
        &empty_seq.vector,
        &fixture["cases"]["empty_string"]["vector"],
        "empty_string",
    );

    // FR query (non-English).
    let fr_seq = adapter
        .embed("comment configurer la persistance ?", Role::Query)
        .unwrap();
    check_vector(
        &fr_seq.vector,
        &fixture["cases"]["fr_query"]["vector"],
        "fr_query",
    );

    // Code snippet.
    let code_seq = adapter
        .embed("fn main() { println!(\"hello\"); }", Role::Passage)
        .unwrap();
    check_vector(
        &code_seq.vector,
        &fixture["cases"]["code_snippet"]["vector"],
        "code_snippet",
    );
}

/// Task 10: unsupported model recipes are rejected with actionable errors.

#[test]
fn t_emb_rejects_unsupported_recipe() {
    let dir = tempfile::tempdir().unwrap();

    // Write a config that looks like BERT but has wrong architecture.
    std::fs::write(
        dir.path().join("config.json"),
        r#"{
                "architectures": ["XLMRobertaModel"],
                "model_type": "xlm-roberta",
                "hidden_size": 768,
                "vocab_size": 250002
            }"#,
    )
    .unwrap();

    let recipe = e5_small_recipe();
    match E5SmallAdapter::validate_config_file(dir.path(), &recipe) {
        Err(ArtifactError::UnsupportedModel(id, reason)) => {
            assert_eq!(id, recipe.id);
            assert!(
                reason.contains("BertModel"),
                "error should mention expected architecture"
            );
        }
        other => panic!("should reject XLMRobertaModel architecture, got: {other:?}"),
    }

    // Correct architecture but wrong tokenizer class.
    std::fs::write(
        dir.path().join("config.json"),
        r#"{
                "architectures": ["BertModel"],
                "model_type": "bert",
                "hidden_size": 384,
                "vocab_size": 250037
            }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("tokenizer_config.json"),
        r#"{
                "tokenizer_class": "BertTokenizer"
            }"#,
    )
    .unwrap();

    match E5SmallAdapter::validate_config_file(dir.path(), &recipe) {
        Err(ArtifactError::UnsupportedModel(_, reason)) => {
            assert!(
                reason.contains("XLMRobertaTokenizer"),
                "error should mention expected tokenizer"
            );
        }
        other => panic!("should reject BertTokenizer class, got: {other:?}"),
    }

    // Correct architecture and tokenizer: passes validation.
    std::fs::write(
        dir.path().join("tokenizer_config.json"),
        r#"{
                "tokenizer_class": "XLMRobertaTokenizer"
            }"#,
    )
    .unwrap();
    assert!(E5SmallAdapter::validate_config_file(dir.path(), &recipe).is_ok());
}

/// T-EMB-03: long-document chunking preserves recall and parent identity.

#[test]
fn t_emb_03_long_document_chunking() {
    let (cache, guard) = test_cache();
    if !populate_cache(&cache, &guard) {
        eprintln!("SKIP: artifacts not present");
        return;
    }
    let mut adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();
    let fixture = load_fixture();

    const COSINE_TOL: f32 = 0.999;

    fn check_vector(actual: &[f32], expected_json: &serde_json::Value, label: &str) {
        let expected: Vec<f32> = expected_json
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_f64())
            .map(|v| v as f32)
            .collect();
        assert_eq!(actual.len(), expected.len(), "{label}: dimension mismatch");
        assert!(
            actual.iter().all(|v| v.is_finite()),
            "{label}: non-finite values"
        );
        let cos: f32 = actual.iter().zip(expected.iter()).map(|(a, b)| a * b).sum();
        assert!(
            cos > COSINE_TOL,
            "{label}: cosine {cos} below tolerance {COSINE_TOL}"
        );
    }

    // The reference fixture was generated with the same recipe as production:
    // prefix = "passage: {title}\n", greedy unit packing under 512 tokens.
    let title = fixture["long_document"]["title"].as_str().unwrap();
    let fragment = fixture["long_document"]["fragment"].as_str().unwrap();

    // Task 6: the Rust chunker must reproduce the reference chunks exactly —
    // same text, same parent-identity offsets (char_start/char_end).
    let rust_chunks = adapter.chunk_passage(title, fragment);
    let ref_chunks = fixture["long_document"]["chunks"].as_array().unwrap();
    assert_eq!(rust_chunks.len(), ref_chunks.len(), "chunk count mismatch");

    for (i, (rc, rf)) in rust_chunks.iter().zip(ref_chunks.iter()).enumerate() {
        assert_eq!(rc.text, rf["text"], "chunk[{i}] text mismatch");
        assert_eq!(
            rc.char_start as u64,
            rf["char_start"].as_u64().unwrap(),
            "chunk[{i}] char_start"
        );
        assert_eq!(
            rc.char_end as u64,
            rf["char_end"].as_u64().unwrap(),
            "chunk[{i}] char_end"
        );
        // Token count includes the bounded prefix.
        let ref_ids: Vec<u32> = rf["input_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_u64())
            .map(|v| v as u32)
            .collect();
        assert_eq!(rc.token_count, ref_ids.len(), "chunk[{i}] token count");

        // Offsets must point at the exact span of this chunk in the fragment.
        let slice = &fragment[rc.char_start..rc.char_end];
        assert_eq!(slice, rc.text, "chunk[{i}] offsets do not match its text");

        // Embedding equivalence: feed the same prefixed input as the reference.
        let full_input = format!("{title}\n{}", rc.text);
        let seq = adapter.embed(&full_input, Role::Passage).unwrap();
        assert_eq!(seq.input_ids, ref_ids, "chunk[{i}] input_ids mismatch");
        check_vector(&seq.vector, &rf["vector"], &format!("long_doc/chunk[{i}]"));
    }

    // The fact is beyond the first model window: it must live in chunk 1.
    let n_chunks = ref_chunks.len();
    assert!(n_chunks >= 2, "fact must be beyond the first window");
    let last_chunk_text = rust_chunks.last().unwrap().text.clone();
    assert!(last_chunk_text.contains("ZEBRA-COPPER-HAMMOCK-7741"));

    // Tail-of-memory retrieval: query for the secret; best match is chunk 1.
    let q_seq = adapter
        .embed("ZEBRA-COPPER-HAMMOCK-7741", Role::Query)
        .unwrap();
    check_vector(
        &q_seq.vector,
        &fixture["cases"]["query_long_fact"]["vector"],
        "query_long_fact",
    );

    let mut best_idx = 0;
    let mut best_cos = f32::MIN;
    for (i, rf) in ref_chunks.iter().enumerate() {
        let expected: Vec<f32> = rf["vector"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_f64())
            .map(|v| v as f32)
            .collect();
        let cos: f32 = q_seq
            .vector
            .iter()
            .zip(expected.iter())
            .map(|(a, b)| a * b)
            .sum();
        if cos > best_cos {
            best_cos = cos;
            best_idx = i;
        }
    }

    assert!(
        best_idx == n_chunks - 1,
        "query should retrieve last chunk (fact location), got {best_idx}, best_cos={best_cos}"
    );
    eprintln!("T-EMB-03: OK — fact in chunk[{best_idx}] of {n_chunks}, cos={best_cos:.4}");
}

/// Task 6 unit checks: offsets, token bounds and determinism (no weights needed).

#[test]
fn t_emb_03_chunking_offsets_and_bounds() {
    let (cache, guard) = test_cache();
    if !populate_cache(&cache, &guard) {
        eprintln!("SKIP: artifacts not present");
        return;
    }
    let mut adapter = E5SmallAdapter::load_from_cache(&cache).unwrap();

    // A title whose prefix alone overflows the window must disclose,
    // never underflow the token budget (usize panic in debug).
    let huge_title = "word ".repeat(600);
    let chunks = adapter.chunk_passage(&huge_title, "small body");
    assert!(
        !chunks.is_empty(),
        "oversized prefix must still disclose content"
    );

    // Short passage fits in one chunk covering the whole fragment.
    let frag = "alpha\nbeta\ngamma";
    let chunks = adapter.chunk_passage("T", frag);
    assert_eq!(chunks.len(), 1);
    assert_eq!((chunks[0].char_start, chunks[0].char_end), (0, frag.len()));

    // Every chunk of a long passage fits the model window when prefixed.
    let unit =
        "Rust ownership rules and borrow checker behavior under concurrent access patterns. ";
    let frag_long = (0..40).map(|_| unit.trim()).collect::<Vec<_>>().join("\n");
    for c in adapter.chunk_passage("Long", &frag_long) {
        assert!(
            c.token_count <= 512,
            "chunk exceeds model window: {}",
            c.token_count
        );
    }

    // Determinism: same input -> identical chunks.
    let a = adapter.chunk_passage("Long", &frag_long);
    let b = adapter.chunk_passage("Long", &frag_long);
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(
            (x.text.as_str(), x.char_start, x.char_end),
            (y.text.as_str(), y.char_start, y.char_end)
        );
    }

    // Coverage: chunks tile the fragment without gaps or overlaps.
    let mut prev_end = 0usize;
    for c in &a {
        assert!(c.char_start >= prev_end, "chunks must not overlap");
        prev_end = c.char_end.max(prev_end);
    }

    // Oversized single unit (one line > 512 tokens) is hard-split at the
    // tokenizer limit: every resulting chunk fits the window when prefixed.
    let word = "token";
    let mut n_words = 1usize;
    loop {
        let t = vec![word; n_words].join(" ");
        if adapter.raw_tokenize(&format!("passage: {t}")).len() > 560 {
            break;
        }
        n_words += 1;
    }
    let huge_line = vec![word; n_words].join(" ");
    let chunks = adapter.chunk_passage("T", &huge_line);
    assert!(
        chunks.len() >= 2,
        "oversized line must be split: {}",
        chunks.len()
    );
    for c in &chunks {
        assert!(
            c.token_count <= 512,
            "hard-split chunk exceeds window: {}",
            c.token_count
        );
        // Verbatim spans of the original fragment.
        assert_eq!(&huge_line[c.char_start..c.char_end], c.text);
    }

    // near-limit case (T-EMB-02): an input at/just under the 512-token model
    // window must embed correctly — finite, unit-normalized, no silent truncation.
    let word = "token";
    let mut n_words = 1usize;
    loop {
        let t = vec![word; n_words].join(" ");
        if adapter.raw_tokenize(&format!("passage: {t}")).len() >= 512 {
            break;
        }
        n_words += 1;
    }
    // Trim whole words until we are at or under the window.
    let mut near_text = vec![word; n_words].join(" ");
    while adapter.raw_tokenize(&format!("passage: {near_text}")).len() > 512 {
        match near_text.rfind(' ') {
            Some(pos) => near_text.truncate(pos),
            None => break,
        }
    }
    let near_tokens = adapter.raw_tokenize(&format!("passage: {near_text}")).len();
    assert!(
        (508..=512).contains(&near_tokens),
        "expected a near-limit input, got {near_tokens} tokens"
    );

    let seq = adapter.embed(&near_text, Role::Passage).unwrap();
    assert_eq!(seq.input_ids.len(), near_tokens);
    assert!(
        seq.vector.iter().all(|v| v.is_finite()),
        "non-finite at window limit"
    );
    let norm_sq: f32 = seq.vector.iter().map(|v| v * v).sum();
    assert!(
        (norm_sq - 1.0).abs() < 1e-4,
        "not unit-normalized at limit (norm^2={norm_sq})"
    );

    // Over-limit input is rejected with an actionable error (no silent truncation).
    let too_long = format!("{near_text} extra word here to push it over the window");
    assert!(adapter.embed(&too_long, Role::Passage).is_err());
}
