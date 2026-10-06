//! P2-2 release evidence (acceptance): the evidence bundle is reproducible
//! from the working tree and tied to the tested source.
//!
//! Quick mode only (fmt + manifest, no full suite): asserts the script exits
//! 0, names its timestamped bundle dir on stdout, and writes a manifest
//! containing the current HEAD SHA, toolchain, lock digest and dep versions
//! — with no absolute local paths (sanitized). Source identity is HEAD;
//! there is deliberately no dirtiness tracking (evidence is generated
//! during fixing rounds, so such a marker would always read true).
//! Full-mode gating (nonzero exit on failed checks) is exercised against
//! mock toolchains in developer probes, not here: this test must stay fast
//! and hermetic.

use std::process::Command;

fn head_sha() -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse must run");
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn quick_evidence_manifest_ties_to_head() {
    let script = Command::new("bash")
        .args(["tools/gen_release_evidence.sh", "--quick"])
        .output()
        .expect("evidence script must be runnable");
    assert!(
        script.status.success(),
        "evidence script failed: {}",
        String::from_utf8_lossy(&script.stderr)
    );
    let stdout = String::from_utf8(script.stdout).unwrap();
    let dir = stdout.lines().last().unwrap_or("").trim().to_string();
    assert!(
        dir.starts_with("reports/release-"),
        "script must print its bundle dir last, got: {stdout}"
    );
    let manifest_path = format!("{dir}/manifest.json");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|_| panic!("manifest must exist at {manifest_path}"));
    let head = head_sha();
    assert!(manifest.contains(&head), "manifest must tie to HEAD {head}");
    assert!(
        dir.contains(&head[..12]) && dir.contains("-quick-"),
        "bundle dir must carry the short SHA and mode, got: {dir}"
    );
    for field in [
        "\"mode\": \"quick\"",
        "\"toolchain\"",
        "\"cargo_lock_sha256\"",
        "\"direct_dependencies\"",
        "\"fmt_clean\"",
    ] {
        assert!(
            manifest.contains(field),
            "manifest must carry {field}:\n{manifest}"
        );
    }
    assert!(
        !manifest.contains("/home/") && !manifest.contains("/Users/"),
        "manifest must be sanitized (no absolute local paths):\n{manifest}"
    );
}

/// Re-review P2-2: two runs in the same timestamp second must not share
/// (and must not overwrite) a run directory — mktemp exclusivity.
#[test]
fn same_second_runs_never_share_a_directory() {
    let run_once = || {
        let script = Command::new("bash")
            .args(["tools/gen_release_evidence.sh", "--quick"])
            .output()
            .expect("evidence script must be runnable");
        assert!(script.status.success());
        String::from_utf8(script.stdout)
            .unwrap()
            .lines()
            .last()
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let first = run_once();
    let second = run_once();
    assert_ne!(first, second, "run directories must be exclusive");
    for dir in [&first, &second] {
        let manifest = std::fs::read_to_string(format!("{dir}/manifest.json"))
            .unwrap_or_else(|_| panic!("manifest must exist at {dir}"));
        assert!(
            manifest.contains(&head_sha()),
            "both manifests must survive intact"
        );
    }
}

/// P2-3: the clean-checkout wrapper must build real CLI flags, never the
/// internal mode name as a flag (`--full` is not a supported flag and the
/// parser exits 2 on it). Static guard: the recursive invocation must not
/// interpolate the mode variable as a flag.
#[test]
fn clean_checkout_never_passes_mode_as_flag() {
    let script = std::fs::read_to_string("tools/gen_release_evidence.sh")
        .expect("evidence script must be readable");
    assert!(
        !script.contains("--$MODE"),
        "recursive call must build --quick/omission explicitly, never --$MODE"
    );
}

/// P2-3: `--quick --clean-checkout` produces a bundle moved back into
/// `reports/` with a HEAD-tied manifest (exercises the wrapper end to
/// end without the minutes-long full suite).
#[test]
fn clean_checkout_quick_produces_bundle() {
    let checkout = tempfile::tempdir().expect("tempdir for clean checkout");
    let checkout_dir = checkout.path().join("checkout");
    let script = Command::new("bash")
        .args([
            "tools/gen_release_evidence.sh",
            "--quick",
            "--clean-checkout",
            checkout_dir.to_str().unwrap(),
        ])
        .output()
        .expect("evidence script must be runnable");
    assert!(
        script.status.success(),
        "clean-checkout quick failed: {}",
        String::from_utf8_lossy(&script.stderr)
    );
    let stdout = String::from_utf8(script.stdout).unwrap();
    let dir = stdout.lines().last().unwrap_or("").trim().to_string();
    assert!(
        dir.starts_with("reports/release-"),
        "script must print its bundle dir last, got: {stdout}"
    );
    let manifest = std::fs::read_to_string(format!("{dir}/manifest.json"))
        .unwrap_or_else(|_| panic!("manifest must exist at {dir}"));
    assert!(
        manifest.contains(&head_sha()),
        "clean-checkout manifest must tie to HEAD"
    );
}
