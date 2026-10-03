//! Deterministic benchmark harness (WP-12; T-BENCH-01/02/03, T-QUALITY-01/02).
//!
//! Contract (RQ-23 reproducible benchmarks):
//! - Every workload is seed-pinned: the same seed always yields the same op
//!   stream and the same probe vectors, so runs are reproducible and raw
//!   histories are comparable across backends and machines.
//! - Storage-only experiments never run model inference inside the measured
//!   boundary: probe vectors are fixed precomputed values, so backend
//!   comparisons cannot hide inference cost inside storage latency.
//! - Every history record carries its op, elapsed micros and success flag;
//!   analysis scripts read the raw JSONL, never aggregate-of-aggregates.
//! - Reports carry provenance (machine label, arch/os/cpu count, workload
//!   config digest, harness version). Unrun workloads carry no values.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

pub mod experiment;
pub mod load;
pub mod quality;

/// Harness version, bumped when the generator or record format changes (old
/// raw histories stay readable; comparisons must match versions).
pub const HARNESS_VERSION: u32 = 2;

/// Crate root for locating tracked test assets (fixtures, artifact probes)
/// and report output dirs. Tests must not depend on the runner's working
/// directory (IDE runners differ from `cargo test`).
pub fn crate_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Deterministic 64-bit generator (xorshift64*; no external dependency, so
/// the workload stream cannot drift with a dependency upgrade).
pub struct WorkloadRng {
    state: u64,
}

impl WorkloadRng {
    /// Seed 0 maps to a fixed nonzero state (a zero xorshift state never moves).
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x9E3779B97F4A7C15 } else { seed },
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Uniform value in `[0, bound)` (`bound` must be positive). Modulo
    /// reduction: deterministic and unbiased enough for workload shaping
    /// (never used for statistical sampling claims).
    pub fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "bound must be positive");
        self.next_u64() % bound
    }
}

/// Storage-only workload op. `Search` carries a probe id, never a live
/// embedding: the measured boundary resolves it through [`probe_vector`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WorkloadOp {
    Put { key: u64, bytes: usize },
    Get { key: u64 },
    Search { probe: u64, top_k: usize },
}

/// Generator configuration (part of the workload config digest).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeneratorConfig {
    /// Workload seed (pins the op stream and probe vectors).
    pub seed: u64,
    /// Puts per 1000 ops (remainder split between get/search).
    pub puts_per_1000: u64,
    /// Key space size (keys uniform in `[0, key_space)`).
    pub key_space: u64,
    /// Value payload bytes per put.
    pub value_bytes: usize,
    /// Candidate count per search op.
    pub top_k: usize,
}

impl Default for GeneratorConfig {
    fn default() -> Self {
        Self {
            seed: 1,
            puts_per_1000: 500,
            key_space: 10_000,
            value_bytes: 256,
            top_k: 10,
        }
    }
}

/// Deterministic op stream over [`GeneratorConfig`].
pub struct OpGenerator {
    rng: WorkloadRng,
    config: GeneratorConfig,
    emitted: u64,
}

impl OpGenerator {
    pub fn new(config: GeneratorConfig) -> Self {
        assert!(config.key_space > 0, "key space must be positive");
        assert!(config.puts_per_1000 <= 1000, "puts share must be per-1000");
        let seed = config.seed;
        Self {
            rng: WorkloadRng::new(seed),
            config,
            emitted: 0,
        }
    }

    pub fn next_op(&mut self) -> WorkloadOp {
        let draw = self.rng.below(1000);
        self.emitted += 1;
        if draw < self.config.puts_per_1000 {
            WorkloadOp::Put {
                key: self.rng.below(self.config.key_space),
                bytes: self.config.value_bytes,
            }
        } else if draw < self.config.puts_per_1000 + (1000 - self.config.puts_per_1000) / 2 {
            WorkloadOp::Get {
                key: self.rng.below(self.config.key_space),
            }
        } else {
            WorkloadOp::Search {
                probe: self.rng.below(self.config.key_space),
                top_k: self.config.top_k,
            }
        }
    }

    pub fn emitted(&self) -> u64 {
        self.emitted
    }
}

/// Fixed precomputed probe vector: dimension `dim`, derived deterministically
/// from `(seed, probe)` and unit-normalized (cosine-ready). Same inputs
/// always yield identical bytes, so the vector is a constant of the
/// workload, not a measurement input.
pub fn probe_vector(seed: u64, probe: u64, dim: usize) -> Vec<f32> {
    let mut rng = WorkloadRng::new(seed ^ probe.wrapping_mul(0x9E3779B97F4A7C15));
    let mut v: Vec<f32> = (0..dim)
        .map(|_| (rng.next_u64() as f64 / u64::MAX as f64) as f32 * 2.0 - 1.0)
        .collect();
    let norm = v
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x = ((*x as f64) / norm) as f32;
        }
    }
    v
}

/// One measured op execution (a JSONL line in the raw history).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryRecord {
    pub seq: u64,
    pub op: WorkloadOp,
    pub elapsed_micros: u64,
    pub ok: bool,
    pub detail: String,
    /// Micros between scheduled arrival and execution start (queue time).
    /// Zero for single-threaded and closed-loop runs, where issue and start
    /// coincide; open-loop runners measure it per op (T-BENCH-02: latency
    /// from scheduled arrival, queue time never omitted).
    pub queued_micros: u64,
}

/// Append-only JSONL history recorder.
pub struct HistoryRecorder {
    file: BufWriter<File>,
    seq: u64,
}

impl HistoryRecorder {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: BufWriter::new(File::create(path)?),
            seq: 0,
        })
    }

    pub fn record(
        &mut self,
        op: WorkloadOp,
        elapsed_micros: u64,
        ok: bool,
        detail: &str,
    ) -> std::io::Result<()> {
        self.record_queued(op, elapsed_micros, 0, ok, detail)
    }

    /// Record with an explicit queue-time component (open-loop runners).
    pub fn record_queued(
        &mut self,
        op: WorkloadOp,
        elapsed_micros: u64,
        queued_micros: u64,
        ok: bool,
        detail: &str,
    ) -> std::io::Result<()> {
        let record = HistoryRecord {
            seq: self.seq,
            op,
            elapsed_micros,
            ok,
            detail: detail.to_string(),
            queued_micros,
        };
        self.seq += 1;
        let mut line = serde_json::to_string(&record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        self.file.write_all(line.as_bytes())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Read a raw history back (analysis scripts and verification tests use this;
/// production code never reads another run's history).
pub fn read_history(path: &Path) -> std::io::Result<Vec<HistoryRecord>> {
    let file = File::open(path)?;
    let mut out = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: HistoryRecord = serde_json::from_str(&line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{path:?} line {index}: {e}"),
            )
        })?;
        out.push(record);
    }
    Ok(out)
}

/// Latency histogram over integer micros (exact percentiles on the raw
/// samples; no bucketing, no lossy compression).
#[derive(Debug, Default)]
pub struct Histogram {
    samples: Vec<u64>,
}

impl Histogram {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, value: u64) {
        self.samples.push(value);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Nearest-rank percentile in `[0, 100]`; `None` when empty.
    pub fn percentile(&self, p: f64) -> Option<u64> {
        if self.samples.is_empty() || !p.is_finite() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
        sorted
            .get(rank.saturating_sub(1).min(sorted.len() - 1))
            .copied()
    }

    pub fn mean(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        Some(self.samples.iter().sum::<u64>() as f64 / self.samples.len() as f64)
    }
}

/// Config digest: hex sha256 over the canonical workload parameters (same
/// config always yields the same digest; it identifies the workload, while
/// the seed identifies the stream instance).
pub fn config_digest(
    seed: u64,
    puts_per_1000: u64,
    key_space: u64,
    value_bytes: usize,
    top_k: usize,
    ops: u64,
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let s = format!("{seed}:{puts_per_1000}:{key_space}:{value_bytes}:{top_k}:{ops}");
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Machine + workload provenance attached to every report (RQ-23: declared
/// hardware and reproducible raw measurements).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Provenance {
    pub machine: String,
    pub arch: String,
    pub os: String,
    pub cpus: usize,
    pub workload_digest: String,
    pub seed: u64,
    pub harness_version: u32,
}

impl Provenance {
    pub fn collect(machine: &str, seed: u64, workload_digest: String) -> Self {
        Self {
            machine: machine.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            workload_digest,
            seed,
            harness_version: HARNESS_VERSION,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same seed replays the identical op stream; a different seed diverges.
    #[test]
    fn same_seed_same_stream() {
        let config = GeneratorConfig {
            seed: 7,
            ..Default::default()
        };
        let mut a = OpGenerator::new(config.clone());
        let mut b = OpGenerator::new(config);
        let stream_a: Vec<WorkloadOp> = (0..500).map(|_| a.next_op()).collect();
        let stream_b: Vec<WorkloadOp> = (0..500).map(|_| b.next_op()).collect();
        assert_eq!(stream_a, stream_b, "seed must pin the stream");
        assert_eq!(a.emitted(), 500);
        let mut c = OpGenerator::new(GeneratorConfig {
            seed: 8,
            ..Default::default()
        });
        let stream_c: Vec<WorkloadOp> = (0..500).map(|_| c.next_op()).collect();
        assert_ne!(stream_a, stream_c, "different seeds must diverge");
    }

    /// Probe vectors are workload constants: identical inputs give identical
    /// bytes, distinct probes give distinct vectors.
    #[test]
    fn probe_vectors_are_fixed() {
        let v1 = probe_vector(7, 42, 8);
        let v2 = probe_vector(7, 42, 8);
        assert_eq!(v1, v2, "same inputs must give identical bytes");
        assert_eq!(v1.len(), 8);
        assert!(v1.iter().all(|x| x.is_finite()), "no NaN/inf: {v1:?}");
        let v3 = probe_vector(7, 43, 8);
        assert_ne!(v1, v3, "distinct probes must differ");
    }

    /// The recorder persists exactly what was measured, in order.
    #[test]
    fn recorder_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut rec = HistoryRecorder::create(&path).unwrap();
        rec.record(WorkloadOp::Put { key: 1, bytes: 4 }, 11, true, "")
            .unwrap();
        rec.record(
            WorkloadOp::Search { probe: 9, top_k: 3 },
            77,
            false,
            "timeout",
        )
        .unwrap();
        rec.flush().unwrap();
        let back = read_history(&path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].seq, 0);
        assert_eq!(
            back[0].op,
            WorkloadOp::Put { key: 1, bytes: 4 },
            "op must survive the round trip"
        );
        assert_eq!(back[0].elapsed_micros, 11);
        assert!(back[0].ok);
        assert_eq!(back[1].seq, 1);
        assert!(!back[1].ok);
        assert_eq!(back[1].detail, "timeout");
    }

    /// Percentiles are exact nearest-rank values on the raw samples.
    #[test]
    fn histogram_percentiles_exact() {
        let mut h = Histogram::new();
        assert!(h.is_empty());
        assert_eq!(h.percentile(99.0), None);
        for v in 1..=100u64 {
            h.push(v);
        }
        assert_eq!(h.len(), 100);
        assert_eq!(h.percentile(50.0), Some(50));
        assert_eq!(h.percentile(95.0), Some(95));
        assert_eq!(h.percentile(99.0), Some(99));
        assert_eq!(h.percentile(100.0), Some(100));
        assert!((h.mean().unwrap() - 50.5).abs() < 1e-9);
    }

    /// Provenance declares the hardware and pins the workload.
    #[test]
    fn provenance_collects_declared_hardware() {
        let p = Provenance::collect("dev-box", 7, "digest-abc".to_string());
        assert!(!p.arch.is_empty(), "arch must be declared");
        assert!(!p.os.is_empty(), "os must be declared");
        assert!(p.cpus >= 1, "cpu count must be declared");
        assert_eq!(p.machine, "dev-box");
        assert_eq!(p.seed, 7);
        assert_eq!(p.workload_digest, "digest-abc");
        assert_eq!(p.harness_version, HARNESS_VERSION);
    }
}
