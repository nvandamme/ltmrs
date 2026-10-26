//! CUDA/GPU inference profile (RQ-09/RQ-10, WP-06 CUDA leg).
//!
//! Device-selection choice (documented per task): the production default
//! stays CPU — `E5SmallAdapter::load_verified` is frozen on `Device::Cpu`
//! and byte-identical with or without the `cuda` feature. The GPU path is
//! reachable ONLY via the explicit `load_verified_on_device` constructor
//! (no env-var implicit switching: an explicit `Device` argument is
//! unambiguous at the call site and cannot leak into daemon defaults).
//!
//! Conventions: artifact gating reuses the `e5_tests` helpers (skip when
//! `tmp/e5-artifacts` is absent, same as the CPU suite). CUDA-device absence
//! is NEVER a silent skip on this path: `cuda_device_or_fail` panics with an
//! actionable message, so a `--features cuda` run without a GPU is LOUD.
//!
//! Fixed corpus (stated, not invented): the four reference-fixture texts
//! (short_en query, fr_query query, code_snippet passage, empty passage)
//! plus the mixed_batch pair. No randomness, no seeds needed.

use super::e5_tests::{populate_cache, test_cache};
use super::{E5SmallAdapter, EmbedInput};
use crate::manifest::e5_small_artifact;
use crate::recipe::Role;
use candle_core::Device;

/// LOUD device acquisition: panics when no CUDA device is present instead
/// of skipping, so `--features cuda` on a GPU-less box cannot go green.
fn cuda_device_or_fail() -> Device {
    Device::new_cuda(0).unwrap_or_else(|e| {
        panic!(
            "CUDA device required for --features cuda GPU tests (Device::new_cuda(0) failed: {e}); \
             run these tests on a CUDA host or drop the `cuda` feature"
        )
    })
}

/// Load the same verified artifacts onto CPU and CUDA for comparison.
fn load_cpu_and_gpu() -> Option<(E5SmallAdapter, E5SmallAdapter)> {
    // Device first: fail loudly before any artifact skip.
    let cuda = cuda_device_or_fail();
    eprintln!("CUDA device under test: {cuda:?}");
    let (cache, guard) = test_cache();
    if !populate_cache(&cache, &guard) {
        eprintln!("SKIP: artifacts not present");
        return None;
    }
    let dir = cache.load_cached(&e5_small_artifact()).unwrap();
    let cpu = E5SmallAdapter::load_verified(&dir).unwrap();
    let gpu = E5SmallAdapter::load_verified_on_device(&dir, &cuda).unwrap();
    Some((cpu, gpu))
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).abs())
        .fold(0f32, f32::max)
}

/// (a) Reference-vector equivalence CPU-vs-GPU on fixed texts.
///
/// Envelope rationale (measured 2026-10-26, RTX 5090 sm_120, CUDA 13.4,
/// candle 0.11.0 F32; per-row values printed by this test): largest observed
/// max-abs diff M = 1.31e-7 (empty-string passage case), smallest observed
/// cosine C = 0.99999982. The committed envelope is `MAX_ABS_DIFF = 1e-6`
/// (~8x the largest observed M: headroom for kernel/clock nondeterminism
/// across runs while staying two orders below the CPU-vs-PyTorch reference
/// tolerance 1e-3) and `COSINE_FLOOR = 0.999_999` (just under the observed
/// minimum; only a genuinely different code path trips it). Neither constant
/// is a round-number guess: both are ceilings over the measured values
/// stated in the cuda report.
#[test]
fn cuda_reference_vector_equivalence_cpu_vs_gpu() {
    const MAX_ABS_DIFF: f32 = 1e-6;
    const COSINE_FLOOR: f32 = 0.999_999;

    let Some((cpu, gpu)) = load_cpu_and_gpu() else {
        return;
    };
    let cases = [
        ("How to configure Fjall persistence mode?", Role::Query),
        ("comment configurer la persistance ?", Role::Query),
        ("fn main() { println!(\"hello\"); }", Role::Passage),
        ("", Role::Passage),
        ("short one", Role::Passage),
        (
            "a somewhat longer sentence about Rust memory models and ownership rules in the compiler",
            Role::Passage,
        ),
    ];
    let inputs: Vec<EmbedInput> = cases
        .iter()
        .map(|(t, r)| EmbedInput {
            text: (*t).to_string(),
            role: *r,
        })
        .collect();
    let cpu_vecs = cpu.embed_batch(&inputs).unwrap();
    let gpu_vecs = gpu.embed_batch(&inputs).unwrap();
    assert_eq!(cpu_vecs.len(), gpu_vecs.len());
    for (i, (c, g)) in cpu_vecs.iter().zip(gpu_vecs.iter()).enumerate() {
        assert_eq!(c.vector.len(), g.vector.len(), "case {i}: dim mismatch");
        assert!(
            g.vector.iter().all(|v| v.is_finite()),
            "case {i}: GPU produced non-finite values"
        );
        let cos = cosine(&c.vector, &g.vector);
        let m = max_abs_diff(&c.vector, &g.vector);
        eprintln!("case {i}: cos={cos:.8}, max_abs_diff={m:.8e}");
        assert!(
            cos >= COSINE_FLOOR,
            "case {i}: cosine {cos} below floor {COSINE_FLOOR}"
        );
        assert!(
            m <= MAX_ABS_DIFF,
            "case {i}: max abs diff {m} above envelope {MAX_ABS_DIFF}"
        );
    }
}

/// (b) Rank-equivalence spot check: same query, top-3 passage ids must be
/// identical whether the vectors came from CPU or CUDA.
#[test]
fn cuda_rank_equivalence_spot_check() {
    let Some((cpu, gpu)) = load_cpu_and_gpu() else {
        return;
    };
    let corpus = [
        "Fjall persistence modes and write-ahead log configuration",
        "how to bake sourdough bread at home",
        "Rust ownership rules and the borrow checker",
        "configuring the Tokio runtime for blocking tasks",
        "pineapple fried rice recipe with cashews",
        "LanceDB vector index maintenance and optimization",
        "the history of the transcontinental railroad",
        "chunking long documents into token windows",
    ];
    let query = "How to configure Fjall persistence mode?";
    let rank = |adapter: &E5SmallAdapter| -> Vec<usize> {
        let q = adapter.embed(query, Role::Query).unwrap().vector;
        let mut scored: Vec<(usize, f32)> = corpus
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let v = adapter.embed(p, Role::Passage).unwrap().vector;
                (i, cosine(&q, &v))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        eprintln!(
            "top-3: {:?}",
            scored[..3]
                .iter()
                .map(|(i, s)| (*i, *s))
                .collect::<Vec<_>>()
        );
        scored.iter().take(3).map(|(i, _)| *i).collect()
    };
    let cpu_top3 = rank(&cpu);
    let gpu_top3 = rank(&gpu);
    assert_eq!(
        cpu_top3, gpu_top3,
        "CPU vs GPU top-3 ranking diverged: {cpu_top3:?} vs {gpu_top3:?}"
    );
}

/// (c) Device presence proof. VRAM figures are stated in the report from
/// `nvidia-smi` (not asserted here: allocator residency is timing-dependent).
#[test]
fn cuda_device_reports() {
    let cuda = cuda_device_or_fail();
    assert!(
        matches!(cuda, Device::Cuda(_)),
        "expected a CUDA device, got {cuda:?}"
    );
    eprintln!("device under test: {cuda:?}");
}
