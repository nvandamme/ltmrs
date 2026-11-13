//! Power rig as code: build the base image, run a trial, or clean a trial.
//!
//! Task 2 owns the runner surface (dispatch validation, dry-run plan,
//! golden guard). The live trial/build/clean bodies land in Tasks 4-5.

/// Reserved base-image name: trials clone FROM it, never run AS it.
const GOLDEN_NAME: &str = "ltmrs-golden";

/// Domain template with `__NAME__`/`__VOLUME__`/`__MAC__` placeholders.
/// Task 4 (live trial body) is the first runtime consumer; tests cover it meanwhile.
#[allow(dead_code)]
const DOMAIN_TEMPLATE: &str = include_str!("../../tools/vm/domain-template.xml");

/// Derive a deterministic guest MAC: QEMU OUI `52:54:00` plus 3 bytes from a
/// stable hash of the trial name (FNV-1a over the name bytes, no dependency).
/// Task 4 (live trial body) is the first runtime consumer; tests cover it meanwhile.
#[allow(dead_code)]
pub fn derived_mac(name: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        (hash >> 16) & 0xff,
        (hash >> 8) & 0xff,
        hash & 0xff
    )
}

/// Stamp the domain template for one trial (no placeholder survives).
/// Task 4 (live trial body) is the first runtime consumer; tests cover it meanwhile.
#[allow(dead_code)]
pub fn stamp_domain(name: &str, volume: &str) -> String {
    let mac = derived_mac(name);
    DOMAIN_TEMPLATE
        .replace("__MAC__", &mac)
        .replace("__VOLUME__", volume)
        .replace("__NAME__", name)
}

/// Guest-side command list for a trial mode (data from `tools/vm/`, not code).
/// Task 4 (live trial body) is the first runtime consumer; tests cover it meanwhile.
#[allow(dead_code)]
pub fn workload_commands(mode: &str) -> Vec<String> {
    let raw = match mode {
        "canary" => include_str!("../../tools/vm/canary-workload.txt"),
        "daemon" => include_str!("../../tools/vm/daemon-workload.txt"),
        _ => panic!("unknown power workload mode '{mode}' (expected canary|daemon)"),
    };
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Build the base image (stub: Task 5 implements).
pub fn run_build(_force: bool, _dry_run: bool) -> Result<i32, String> {
    Err("not implemented".to_string())
}

/// Outcome of one trial step: the captured exit code plus the stderr tail.
pub struct StepOutcome {
    /// Step name (e.g. `vol-clone`, `define`, `workload`).
    pub name: String,
    /// Process exit code for the step's control-plane spawn.
    pub exit_code: i32,
    /// Last 4 KiB of the step's stderr (harness fault on `Err`).
    pub stderr_tail: String,
}

/// A passed step with no stderr (verdict-schema tests, poll success).
pub fn step_ok(name: &str) -> StepOutcome {
    StepOutcome {
        name: name.to_string(),
        exit_code: 0,
        stderr_tail: String::new(),
    }
}

/// Build the trial verdict value written to `verdict.json`.
///
/// The volume is kept for forensics exactly when the verdict is not `pass`.
pub fn build_verdict(
    mode: &str,
    name: &str,
    verdict: &str,
    steps: Vec<StepOutcome>,
) -> serde_json::Value {
    let steps: Vec<serde_json::Value> = steps
        .iter()
        .map(|step| {
            serde_json::json!({
                "name": step.name,
                "exit_code": step.exit_code,
                "stderr_tail": step.stderr_tail,
            })
        })
        .collect();
    serde_json::json!({
        "mode": mode,
        "name": name,
        "verdict": verdict,
        "steps": steps,
        "kept_volume": verdict != "pass",
    })
}

/// Single cfg-gated spawn site for every hypervisor control-plane call.
///
/// Linux spawns `virsh`/`ssh`/`qemu-img` via `run.rs` (no shell); other
/// platforms report the missing Hyper-V CLI backend instead of spawning.
#[cfg(target_os = "linux")]
fn spawn_control(program: &str, args: &[String]) -> crate::run::RunOutcome {
    crate::run::execute(program, args, std::path::Path::new("."))
}

/// Single cfg-gated spawn site for every hypervisor control-plane call.
///
/// Non-Linux stub: reports the missing Hyper-V CLI backend (never spawns).
#[cfg(not(target_os = "linux"))]
fn spawn_control(program: &str, _args: &[String]) -> crate::run::RunOutcome {
    crate::run::RunOutcome {
        exit_code: 127,
        stdout_tail: String::new(),
        stderr_tail: format!(
            "unsupported platform — Hyper-V CLI backend not yet ported (would spawn '{program}')"
        ),
        started_ms: 0,
        finished_ms: 0,
    }
}

/// Fold one spawn into a step: harness spawn failures are `Err`
/// (→ `inconclusive`), any other exit — zero or not — is `Ok`.
fn outcome_of(name: &str, outcome: &crate::run::RunOutcome) -> Result<StepOutcome, String> {
    if outcome.exit_code == 127 && outcome.stderr_tail.starts_with("failed to spawn") {
        return Err(format!(
            "step '{name}' harness fault: {}",
            outcome.stderr_tail
        ));
    }
    Ok(StepOutcome {
        name: name.to_string(),
        exit_code: outcome.exit_code,
        stderr_tail: outcome.stderr_tail.clone(),
    })
}

/// Clone the golden volume for one trial.
fn step_vol_clone(volume: &str) -> Result<StepOutcome, String> {
    let argv = vec![
        "vol-clone".to_string(),
        format!("{GOLDEN_NAME}-vol"),
        volume.to_string(),
        "--pool".to_string(),
        "default".to_string(),
    ];
    outcome_of("vol-clone", &spawn_control("virsh", &argv))
}

/// Stamp the domain template and define the trial domain.
fn step_define(
    guest: &str,
    volume: &str,
    xml_path: &std::path::Path,
) -> Result<StepOutcome, String> {
    let xml = stamp_domain(guest, volume);
    if let Some(parent) = xml_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("step 'define' harness fault: cannot create dir: {err}"))?;
    }
    std::fs::write(xml_path, xml)
        .map_err(|err| format!("step 'define' harness fault: cannot write XML: {err}"))?;
    let argv = vec!["define".to_string(), xml_path.display().to_string()];
    outcome_of("define", &spawn_control("virsh", &argv))
}

/// Boot (`create`) the trial domain from its XML; `step_name` is
/// `create` on first boot and `recreate` after the power cut.
fn step_create(xml_path: &std::path::Path, step_name: &str) -> Result<StepOutcome, String> {
    let argv = vec!["create".to_string(), xml_path.display().to_string()];
    outcome_of(step_name, &spawn_control("virsh", &argv))
}

/// Poll SSH until the guest answers or the 60s deadline expires.
fn step_ssh_wait(guest: &str) -> Result<StepOutcome, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let argv = vec![
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=2".to_string(),
            guest.to_string(),
            "true".to_string(),
        ];
        let outcome = spawn_control("ssh", &argv);
        if outcome.exit_code == 0 {
            return Ok(step_ok("ssh-wait"));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "step 'ssh-wait' harness fault: guest '{guest}' unreachable after 60s: {}",
                outcome.stderr_tail
            ));
        }
        // 2s between probes: guest boot takes tens of seconds, so a shorter
        // interval only burns SSH handshakes without reaching ready sooner.
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// Run the mode workload lines over the `ssh` CLI (one spawn per line).
fn step_workload(mode: &str, guest: &str) -> Result<StepOutcome, String> {
    let mut tails: Vec<String> = Vec::new();
    for line in workload_commands(mode) {
        let argv = vec![guest.to_string(), line];
        let step = outcome_of("workload", &spawn_control("ssh", &argv))?;
        if !step.stderr_tail.is_empty() {
            tails.push(step.stderr_tail.clone());
        }
        if step.exit_code != 0 {
            return Ok(step);
        }
    }
    Ok(StepOutcome {
        name: "workload".to_string(),
        exit_code: 0,
        stderr_tail: tails.join("\n"),
    })
}

/// Cut power to the trial domain.
fn step_destroy(guest: &str) -> Result<StepOutcome, String> {
    let argv = vec!["destroy".to_string(), guest.to_string()];
    outcome_of("destroy", &spawn_control("virsh", &argv))
}

/// Guest-side verify commands per mode (receipt/file marker + journal).
fn verify_commands(mode: &str) -> Vec<String> {
    if mode == "canary" {
        vec!["cat /var/lib/power-canary".to_string(), "dmesg".to_string()]
    } else {
        vec![
            "cat /var/lib/ltmrs/receipts.json".to_string(),
            "dmesg".to_string(),
        ]
    }
}

/// Verify durability after the power cut, over the `ssh` CLI.
fn step_verify(mode: &str, guest: &str) -> Result<StepOutcome, String> {
    let mut tails: Vec<String> = Vec::new();
    for line in verify_commands(mode) {
        let argv = vec![guest.to_string(), line];
        let step = outcome_of("verify", &spawn_control("ssh", &argv))?;
        if !step.stderr_tail.is_empty() {
            tails.push(step.stderr_tail.clone());
        }
        if step.exit_code != 0 {
            return Ok(step);
        }
    }
    Ok(StepOutcome {
        name: "verify".to_string(),
        exit_code: 0,
        stderr_tail: tails.join("\n"),
    })
}

/// Record one step; returns true when the pipeline must stop.
///
/// Precedence is `inconclusive` > `fail` > `pass`: a later step never
/// downgrades an earlier verdict.
fn push_step(
    steps: &mut Vec<StepOutcome>,
    verdict: &mut String,
    name: &str,
    result: Result<StepOutcome, String>,
) -> bool {
    match result {
        Ok(step) => {
            let bad = step.exit_code != 0;
            if bad && *verdict == "pass" {
                *verdict = "fail".to_string();
            }
            steps.push(step);
            bad
        }
        Err(tail) => {
            *verdict = "inconclusive".to_string();
            steps.push(StepOutcome {
                name: name.to_string(),
                exit_code: 127,
                stderr_tail: tail,
            });
            true
        }
    }
}

/// Live trial body: clone → define → create → SSH wait → workload →
/// destroy → recreate → verify → verdict (volume kept iff not `pass`).
fn run_trial_live(mode: &str, name: &str) -> Result<i32, String> {
    if cfg!(not(target_os = "linux")) {
        return Err("unsupported platform — Hyper-V CLI backend not yet ported".to_string());
    }
    let guest = format!("power-{name}");
    let volume = format!("{guest}.img");
    let dir = std::path::PathBuf::from(format!("reports/power-{name}"));
    let xml_path = dir.join(format!("{guest}.xml"));

    let mut steps: Vec<StepOutcome> = Vec::new();
    let mut verdict = "pass".to_string();

    let mut stop = push_step(
        &mut steps,
        &mut verdict,
        "vol-clone",
        step_vol_clone(&volume),
    );
    if !stop {
        stop = push_step(
            &mut steps,
            &mut verdict,
            "define",
            step_define(&guest, &volume, &xml_path),
        );
    }
    let mut domain_may_exist = false;
    if !stop {
        domain_may_exist = true;
        stop = push_step(
            &mut steps,
            &mut verdict,
            "create",
            step_create(&xml_path, "create"),
        );
    }
    if !stop {
        stop = push_step(&mut steps, &mut verdict, "ssh-wait", step_ssh_wait(&guest));
    }
    if !stop {
        stop = push_step(
            &mut steps,
            &mut verdict,
            "workload",
            step_workload(mode, &guest),
        );
    }
    if domain_may_exist {
        stop = push_step(&mut steps, &mut verdict, "destroy", step_destroy(&guest)) || stop;
    }
    if !stop {
        stop = push_step(
            &mut steps,
            &mut verdict,
            "recreate",
            step_create(&xml_path, "recreate"),
        );
    }
    if !stop {
        stop = push_step(
            &mut steps,
            &mut verdict,
            "verify",
            step_verify(mode, &guest),
        );
    }
    let _ = stop;

    let verdict_value = build_verdict(mode, name, &verdict, steps);
    let text = serde_json::to_string_pretty(&verdict_value)
        .map_err(|err| format!("failed to serialize verdict for trial '{name}': {err}"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    std::fs::write(dir.join("verdict.json"), text)
        .map_err(|err| format!("failed to write verdict for trial '{name}': {err}"))?;
    Ok(if verdict == "pass" { 0 } else { 1 })
}

/// Would-be control-plane argv vectors (program first) for a trial.
///
/// Generated from the same per-step commands the live body runs
/// (clone → define → create → ssh-wait → workload lines → destroy →
/// recreate → verify lines) so the preview cannot drift from the pipeline.
fn trial_plan(mode: &str, name: &str) -> Vec<Vec<String>> {
    let guest = format!("power-{name}");
    let mut plan = vec![
        vec![
            "virsh".to_string(),
            "vol-clone".to_string(),
            format!("{GOLDEN_NAME}-vol"),
            format!("{guest}.img"),
            "--pool".to_string(),
            "default".to_string(),
        ],
        vec![
            "virsh".to_string(),
            "define".to_string(),
            format!("{guest}.xml"),
        ],
        vec![
            "virsh".to_string(),
            "create".to_string(),
            format!("{guest}.xml"),
        ],
        vec!["ssh".to_string(), guest.clone(), "true".to_string()],
    ];
    for line in workload_commands(mode) {
        plan.push(vec!["ssh".to_string(), guest.clone(), line]);
    }
    plan.push(vec![
        "virsh".to_string(),
        "destroy".to_string(),
        guest.clone(),
    ]);
    plan.push(vec![
        "virsh".to_string(),
        "create".to_string(),
        format!("{guest}.xml"),
    ]);
    for line in verify_commands(mode) {
        plan.push(vec!["ssh".to_string(), guest.clone(), line]);
    }
    plan
}

/// Run a trial: dry-run prints the plan without spawning; otherwise the
/// live clone → boot → SSH → cut → verify → verdict pipeline runs.
pub fn run_trial(mode: &str, name: &str, _force: bool, dry_run: bool) -> Result<i32, String> {
    if mode != "canary" && mode != "daemon" {
        return Err(format!(
            "unknown power trial mode '{mode}' (expected canary|daemon)"
        ));
    }
    if name == GOLDEN_NAME {
        return Err(format!(
            "refusing to run trial as reserved base image '{GOLDEN_NAME}' (golden guard)"
        ));
    }
    if name.contains('/') || name.contains("..") {
        return Err(format!(
            "invalid power trial name '{name}' (must not contain '/' or '..')"
        ));
    }
    if dry_run {
        for argv in trial_plan(mode, name) {
            crate::verbs::print_dry_run(&argv[0], &argv[1..]);
        }
        return Ok(0);
    }
    run_trial_live(mode, name)
}

/// Clean a trial (stub: Task 5 implements; the golden guard already holds).
pub fn run_clean(name: &str, _force: bool, _dry_run: bool) -> Result<i32, String> {
    if name == GOLDEN_NAME {
        return Err(format!(
            "refusing to clean reserved base image '{GOLDEN_NAME}' (golden guard)"
        ));
    }
    Err("not implemented".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn power_trial_dry_run_prints_without_spawning() {
        let code = crate::power::run_trial("canary", "t1", false, true).unwrap();
        assert_eq!(code, 0);
        assert!(!std::path::Path::new("reports/power-t1").exists());
    }

    #[test]
    fn power_trial_rejects_golden_name() {
        let err = crate::power::run_trial("canary", "ltmrs-golden", false, true).unwrap_err();
        assert!(
            err.contains("ltmrs-golden"),
            "must name the guard target: {err}"
        );
    }

    #[test]
    fn power_trial_rejects_unknown_mode() {
        let err = crate::power::run_trial("bogus", "t1", false, true).unwrap_err();
        assert!(err.contains("bogus"), "must name the mode: {err}");
    }

    #[test]
    fn power_trial_rejects_path_escape_name() {
        for bad in ["../evil", "a/b"] {
            let err = crate::power::run_trial("canary", bad, false, true).unwrap_err();
            assert!(err.contains(bad), "must name the bad name: {err}");
        }
    }

    #[test]
    fn power_trial_plan_matches_live_step_sequence() {
        let plan = super::trial_plan("canary", "t1");
        let names: Vec<&str> = plan
            .iter()
            .map(|argv| {
                if argv[0] == "virsh" {
                    argv[1].as_str()
                } else {
                    "ssh"
                }
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "vol-clone",
                "define",
                "create",
                "ssh",
                "ssh",
                "ssh",
                "ssh",
                "destroy",
                "create",
                "ssh",
                "ssh"
            ]
        );
    }

    #[test]
    fn power_dispatch_trial_dry_run_returns_zero() {
        let code =
            crate::verbs::run_power("trial", Some("canary"), Some("t1"), false, true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn power_dispatch_rejects_trial_without_mode() {
        let err = crate::verbs::run_power("trial", None, Some("t1"), false, true).unwrap_err();
        assert!(err.contains("--mode"), "unexpected error: {err}");
    }

    #[test]
    fn power_dispatch_rejects_trial_without_name() {
        let err = crate::verbs::run_power("trial", Some("canary"), None, false, true).unwrap_err();
        assert!(err.contains("--name"), "unexpected error: {err}");
    }

    #[test]
    fn power_dispatch_rejects_clean_without_name() {
        let err = crate::verbs::run_power("clean", None, None, false, true).unwrap_err();
        assert!(err.contains("--name"), "unexpected error: {err}");
    }

    #[test]
    fn power_dispatch_rejects_unknown_op() {
        let err = crate::verbs::run_power("frobnicate", None, None, false, true).unwrap_err();
        assert!(err.contains("frobnicate"), "must name the op: {err}");
        assert!(
            err.contains("build|trial|clean"),
            "must list the ops: {err}"
        );
    }

    #[test]
    fn stamp_domain_fills_placeholders_with_derived_mac() {
        let xml = crate::power::stamp_domain("wp12-03", "wp12-03.qcow2");
        assert!(xml.contains("<name>wp12-03</name>"));
        assert!(xml.contains("wp12-03.qcow2"));
        assert!(
            !xml.contains("__NAME__") && !xml.contains("__VOLUME__") && !xml.contains("__MAC__")
        );
        let mac1 = crate::power::derived_mac("wp12-03");
        let mac2 = crate::power::derived_mac("wp12-03");
        assert_eq!(mac1, mac2);
        assert!(mac1.starts_with("52:54:00"));
    }

    #[test]
    fn workload_commands_returns_per_mode_lists() {
        let canary = crate::power::workload_commands("canary");
        assert!(!canary.is_empty());
        let daemon = crate::power::workload_commands("daemon");
        assert!(!daemon.is_empty());
        assert_ne!(canary, daemon);
    }

    #[test]
    fn verdict_json_has_required_fields() {
        let v = crate::power::build_verdict(
            "canary",
            "t1",
            "pass",
            vec![crate::power::step_ok("boot")],
        );
        let text = serde_json::to_string(&v).unwrap();
        for field in ["mode", "name", "verdict", "steps", "kept_volume"] {
            assert!(text.contains(field), "verdict missing {field}");
        }
    }

    #[test]
    fn verdict_keeps_volume_iff_not_pass() {
        let pass = crate::power::build_verdict(
            "canary",
            "t1",
            "pass",
            vec![crate::power::step_ok("boot")],
        );
        assert_eq!(pass["kept_volume"], false);
        let fail = crate::power::build_verdict(
            "canary",
            "t1",
            "fail",
            vec![crate::power::step_ok("boot")],
        );
        assert_eq!(fail["kept_volume"], true);
        let inconclusive = crate::power::build_verdict(
            "canary",
            "t1",
            "inconclusive",
            vec![crate::power::step_ok("boot")],
        );
        assert_eq!(inconclusive["kept_volume"], true);
    }
    #[test]
    #[should_panic(expected = "unknown power workload mode")]
    fn workload_commands_rejects_unknown_mode() {
        crate::power::workload_commands("bogus");
    }
}
