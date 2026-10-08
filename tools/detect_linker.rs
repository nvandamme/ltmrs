// Linker auto-detection for ltmrs workspace members (build.rs support).
//
// Each member's `build.rs` is three lines including this file:
// probe for a fast linker, emit `-fuse-ld=<it>` for that package's own
// targets, else emit nothing (graceful fallback to the default linker
// so fresh checkouts and CI build without any setup).
//
// Selection, in order:
// 1. `LTMRS_LINKER` env (`mold`, `wild`, `system`, …): explicit choice,
//    also forces a rebuild when changed (`rerun-if-env-changed` below).
// 2. Auto-detect: first of `mold`, `wild` that the system driver
//    accepts (a real link probe, not just PATH presence — an old gcc
//    may know the binary name but reject the flag).
// 3. Nothing: default linker, build stays correct, just slower.
//
// Linux-only by construction: elsewhere we emit nothing (Apple Clang
// and MSVC don't take `-fuse-ld`, and probing them would be pointless).
// `cargo::rustc-link-arg` does NOT propagate to dependents (verified
// 2026-10-08 with a two-crate probe), hence one shim per member.

fn main() {
    println!("cargo::rerun-if-env-changed=LTMRS_LINKER");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let choice = std::env::var("LTMRS_LINKER").unwrap_or_default();
    let linker: Option<String> = match choice.as_str() {
        "" => detect(),
        "system" => None,
        other => Some(other.to_string()),
    };
    if let Some(linker) = linker {
        println!("cargo::rustc-link-arg=-fuse-ld={linker}");
    }
}

/// First of mold/wild that the system driver accepts for a real link.
/// Spawns at most two short-lived processes per build-script run (which
/// itself only reruns when the package rebuilds).
fn detect() -> Option<String> {
    use std::io::Write;
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let dir = std::env::temp_dir();
    for candidate in ["mold", "wild"] {
        // PID-suffixed: cargo runs member build scripts in parallel and
        // they must not share probe paths.
        let probe = dir.join(format!(
            "ltmrs-link-probe-{}-{}",
            candidate,
            std::process::id()
        ));
        let mut child = match std::process::Command::new(&cc)
            .arg(format!("-fuse-ld={candidate}"))
            .args(["-x", "c", "-", "-o"])
            .arg(&probe)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => continue,
        };
        let wrote = child
            .stdin
            .as_mut()
            .map(|s| s.write_all(b"int main(){return 0;}").is_ok())
            .unwrap_or(false);
        let ok = wrote
            && matches!(child.wait(), Ok(status) if status.success())
            && probe.exists();
        let _ = std::fs::remove_file(&probe);
        if ok {
            return Some(candidate.to_string());
        }
    }
    None
}
