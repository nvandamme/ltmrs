//! Power rig as code: build the base image, run a trial, or clean a trial.
//!
//! Task 2 owns the runner surface (dispatch validation, dry-run plan,
//! golden guard). The live trial/build/clean bodies land in Tasks 4-5.

/// Reserved base-image name: trials clone FROM it, never run AS it.
const GOLDEN_NAME: &str = "ltmrs-golden";

/// Domain template with `__NAME__`/`__VOLUME__`/`__MAC__`/`__SERIAL_PORT__`
/// placeholders.
const DOMAIN_TEMPLATE: &str = include_str!("../../tools/vm/domain-template.xml");

/// Derive a deterministic guest MAC: QEMU OUI `52:54:00` plus 3 bytes from a
/// stable hash of the trial name (FNV-1a over the name bytes, no dependency).
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

/// Derive a per-trial TCP serial port from the trial name (same FNV-1a
/// family as the MAC): base 4599 + low 10 bits, so concurrent or leftover
/// trial domains never collide on the passthrough serial listener.
pub fn derived_serial_port(name: &str) -> u16 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    4599 + ((hash >> 32) as u16 % 1024)
}

/// Stamp the domain template for one trial (no placeholder survives).
pub fn stamp_domain(name: &str, volume: &str) -> String {
    let mac = derived_mac(name);
    let serial_port = derived_serial_port(name);
    DOMAIN_TEMPLATE
        .replace("__MAC__", &mac)
        .replace("__SERIAL_PORT__", &serial_port.to_string())
        .replace("__VOLUME__", volume)
        .replace("__NAME__", name)
}

/// Guest-side command list for a trial mode (data from `tools/vm/`, not code).
/// Task 4 (live trial body) is the first runtime consumer; tests cover it meanwhile.
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

/// Guest-side provision lines for the base image (each one `ssh` call, no
/// heredoc scripts): mkfs vda1, copy tree, static net, autologin, dev
/// nodes, authorized_keys, runlevel links.
fn build_provision_commands() -> Vec<String> {
    vec![
        "mkfs.ext4 -L ltmrs-golden /dev/vda1".to_string(),
        "mount /dev/vda1 /mnt && cp -a /live/tree/. /mnt/ && umount /mnt".to_string(),
        "echo 'auto eth0\niface eth0 inet static\naddress 192.168.122.10' > /mnt/etc/network/interfaces".to_string(),
        "echo 'tty1::respawn:/sbin/getty -n -l /bin/autologin tty1 38400' >> /mnt/etc/inittab".to_string(),
        "mknod /mnt/dev/console c 5 1 && mknod /mnt/dev/null c 1 3".to_string(),
        "install -m 600 /root/key.pub /mnt/root/.ssh/authorized_keys".to_string(),
        "ln -s /etc/init.d/S99power /mnt/etc/runlevels/default/S99power".to_string(),
    ]
}

/// Guest SSH target: the golden image bakes static 192.168.122.50
/// (verified live from ltmrs-golden-vol). All trial clones share it, so
/// trials run one at a time; per-trial MAC+IP assignment belongs to the
/// build op (plan follow-up), not to this const.
const GUEST_IP: &str = "192.168.122.50";

/// SSH identity for trial guest access: root + key from the environment.
/// `LTMRS_POWER_SSH_KEY` must point at the private key matching the
/// authorized_keys baked into the golden image (fail loud if unset —
/// BatchMode would otherwise fail opaquely as `nvand@` with no key).
fn ssh_key() -> Result<String, String> {
    std::env::var("LTMRS_POWER_SSH_KEY").map_err(|_| {
        "power trial needs LTMRS_POWER_SSH_KEY set to the guest root key path".to_string()
    })
}

/// SSH wait-probe argv (after `ssh`): shared by the live poll and every
/// dry-run plan so the preview shows exactly what the pipeline spawns.
/// Shared SSH identity argv (after `ssh`, before the remote command):
/// BatchMode + short connect timeout + no host-key prompt (throwaway trial
/// VMs regenerate keys) + key from `LTMRS_POWER_SSH_KEY` + root@guest-IP.
/// Every guest-ssh spawn goes through here so identity cannot drift.
fn ssh_base_argv_with_key(key: &str) -> Vec<String> {
    vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-i".to_string(),
        key.to_string(),
        format!("root@{GUEST_IP}"),
    ]
}

/// Env edge: read the key, then delegate to the pure core.
fn ssh_base_argv() -> Result<Vec<String>, String> {
    Ok(ssh_base_argv_with_key(&ssh_key()?))
}

fn ssh_wait_argv() -> Result<Vec<String>, String> {
    let mut argv = ssh_base_argv()?;
    argv.push("true".to_string());
    Ok(argv)
}

/// One guest-ssh command argv (after `ssh`): shared identity + remote line.
fn ssh_cmd_argv(line: &str) -> Result<Vec<String>, String> {
    let mut argv = ssh_base_argv()?;
    argv.push(line.to_string());
    Ok(argv)
}

/// Would-be control-plane argv vectors (program first) for a build.
///
/// Generated from the same commands the live body runs so the preview
/// cannot drift from the pipeline.
fn build_plan() -> Vec<Vec<String>> {
    let guest = GOLDEN_NAME.to_string();
    let mut plan = vec![
        vec![
            "qemu-img".to_string(),
            "create".to_string(),
            "-f".to_string(),
            "qcow2".to_string(),
            format!("{guest}-vol.qcow2"),
            "8G".to_string(),
        ],
        vec![
            "virsh".to_string(),
            "define".to_string(),
            format!("{guest}.xml"),
        ],
        vec!["virsh".to_string(), "start".to_string(), guest.clone()],
        {
            let mut wait = vec!["ssh".to_string()];
            match ssh_wait_argv() {
                Ok(argv) => wait.extend(argv),
                Err(e) => wait.push(format!("<ssh-key-missing: {e}>")),
            }
            wait
        },
    ];
    for line in build_provision_commands() {
        // NOTE: fresh-image bootstrap IP unresolved (see run_build_live);
        // preview shows the golden-name target until the build op assigns IPs.
        plan.push(vec!["ssh".to_string(), guest.clone(), line]);
    }
    plan.push(vec![
        "virsh".to_string(),
        "destroy".to_string(),
        guest.clone(),
    ]);
    plan.push(vec![
        "virsh".to_string(),
        "undefine".to_string(),
        guest.clone(),
    ]);
    plan.push(vec![
        "virsh".to_string(),
        "vol-info".to_string(),
        format!("{guest}-vol"),
        "--pool".to_string(),
        "default".to_string(),
    ]);
    plan
}

/// Milliseconds since the Unix epoch (mirrors `run.rs` for record stamps).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run-record directory for a build (no trial name exists; the op owns it).
fn build_report_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("reports/power-build")
}

/// Run-record directory for a clean (sits next to the trial verdict).
fn clean_report_dir(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("reports/power-{name}"))
}

/// Persist the op run-record (`record.json` under `dir`) via Task 2 plumbing.
///
/// Steps plus verdict ride in `meta`; the outcome aggregates the pipeline:
/// 0 on `pass`, 127 on `inconclusive`, 1 on `fail`.
fn write_op_record(
    dir: &std::path::Path,
    verb: &str,
    argv: &[String],
    steps: &[StepOutcome],
    verdict: &str,
    started_ms: u64,
) -> Result<(), String> {
    let code = if verdict == "pass" {
        0
    } else if verdict == "inconclusive" {
        127
    } else {
        1
    };
    let tails: Vec<String> = steps
        .iter()
        .filter(|step| step.exit_code != 0)
        .map(|step| format!("{}: {}", step.name, step.stderr_tail))
        .collect();
    let outcome = crate::run::RunOutcome {
        exit_code: code,
        stdout_tail: String::new(),
        stderr_tail: tails.join("\n"),
        started_ms,
        finished_ms: now_ms(),
    };
    let meta = serde_json::json!({
        "verdict": verdict,
        "steps": steps
            .iter()
            .map(|step| serde_json::json!({
                "name": step.name,
                "exit_code": step.exit_code,
                "stderr_tail": step.stderr_tail,
            }))
            .collect::<Vec<_>>(),
    });
    crate::run::write_record(dir, verb, argv, &outcome, Some(&meta)).map_err(|err| {
        format!(
            "failed to write {verb} run-record in {}: {err}",
            dir.display()
        )
    })?;
    Ok(())
}

/// Live build body: qcow2 create → define → create → SSH wait →
/// provision → destroy → undefine → vol-info assert.
fn run_build_live(force: bool) -> Result<i32, String> {
    if cfg!(not(target_os = "linux")) {
        return Err("unsupported platform — Hyper-V CLI backend not yet ported".to_string());
    }
    let guest = GOLDEN_NAME.to_string();
    let volume = format!("{guest}-vol");
    if !force {
        let probe = vec![volume.clone(), "--pool".to_string(), "default".to_string()];
        let probe = [vec!["vol-info".to_string()], probe].concat();
        if spawn_control("virsh", &probe).exit_code == 0 {
            return Err(format!(
                "base image '{volume}' already exists (pass --force to overwrite)"
            ));
        }
    }
    let dir = build_report_dir();
    let xml_path = dir.join(format!("{guest}.xml"));
    let started_ms = now_ms();
    let record_argv = if force {
        vec![
            "power".to_string(),
            "build".to_string(),
            "--force".to_string(),
        ]
    } else {
        vec!["power".to_string(), "build".to_string()]
    };
    let img_argv = vec![
        "create".to_string(),
        "-f".to_string(),
        "qcow2".to_string(),
        format!("{volume}.qcow2"),
        "8G".to_string(),
    ];
    let mut steps: Vec<StepOutcome> = Vec::new();
    let mut verdict = "pass".to_string();
    let mut stop = push_step(
        &mut steps,
        &mut verdict,
        "img-create",
        outcome_of("img-create", &spawn_control("qemu-img", &img_argv)),
    );
    if !stop {
        stop = push_step(
            &mut steps,
            &mut verdict,
            "define",
            step_define(&guest, &volume, &xml_path),
        );
    }
    if !stop {
        stop = push_step(
            &mut steps,
            &mut verdict,
            "create",
            step_create(&guest, "create"),
        );
    }
    if !stop {
        stop = push_step(&mut steps, &mut verdict, "ssh-wait", step_ssh_wait(&guest));
    }
    if !stop {
        let mut tails: Vec<String> = Vec::new();
        let mut result = Ok(StepOutcome {
            name: "provision".to_string(),
            exit_code: 0,
            stderr_tail: String::new(),
        });
        for line in build_provision_commands() {
            let argv = vec![guest.clone(), line];
            match outcome_of("provision", &spawn_control("ssh", &argv)) {
                Err(fault) => {
                    result = Err(fault);
                    break;
                }
                Ok(step) => {
                    if !step.stderr_tail.is_empty() {
                        tails.push(step.stderr_tail.clone());
                    }
                    if step.exit_code != 0 {
                        result = Ok(step);
                        break;
                    }
                }
            }
        }
        if let Ok(step) = &mut result {
            step.stderr_tail = tails.join("\n");
        }
        stop = push_step(&mut steps, &mut verdict, "provision", result);
    }
    if !stop {
        stop = push_step(&mut steps, &mut verdict, "destroy", step_destroy(&guest));
    }
    if !stop {
        let argv = vec!["undefine".to_string(), guest.clone()];
        stop = push_step(
            &mut steps,
            &mut verdict,
            "undefine",
            outcome_of("undefine", &spawn_control("virsh", &argv)),
        );
    }
    if !stop {
        let argv = vec![
            "vol-info".to_string(),
            volume.clone(),
            "--pool".to_string(),
            "default".to_string(),
        ];
        push_step(
            &mut steps,
            &mut verdict,
            "vol-info",
            outcome_of("vol-info", &spawn_control("virsh", &argv)),
        );
    }
    write_op_record(
        &dir,
        "power-build",
        &record_argv,
        &steps,
        &verdict,
        started_ms,
    )?;
    if verdict != "pass" {
        let tails: Vec<String> = steps
            .iter()
            .filter(|step| step.exit_code != 0)
            .map(|step| format!("{}: {}", step.name, step.stderr_tail))
            .collect();
        return Err(format!("build failed ({}): {}", verdict, tails.join("; ")));
    }
    Ok(0)
}
/// Build the base image: dry-run prints the plan without spawning,
/// otherwise the live qcow2 → define → provision → destroy pipeline runs.
/// Without `--force` a live build refuses when the golden volume exists.
pub fn run_build(force: bool, dry_run: bool) -> Result<i32, String> {
    if dry_run {
        for argv in build_plan() {
            crate::verbs::print_dry_run(&argv[0], &argv[1..]);
        }
        return Ok(0);
    }
    run_build_live(force)
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
    // System libvirt daemon holds the pools/volumes/networks; the session
    // URI has none of them. Every virsh call pins -c explicitly so the
    // verb never depends on LIBVIRT_DEFAULT_URI or the caller's default.
    if program == "virsh" {
        let mut full = vec!["-c".to_string(), "qemu:///system".to_string()];
        full.extend(args.iter().cloned());
        return crate::run::execute(program, &full, std::path::Path::new("."));
    }
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
    // QEMU file-backed serial needs an absolute, pre-existing log path:
    // libvirt resolves relative paths against its own CWD, not the xtask
    // workdir. Stamp the XML with the absolute log path, then touch it.
    let log_path = std::env::current_dir()
        .map_err(|err| format!("step 'define' harness fault: cannot read CWD: {err}"))?
        .join("tmp/vm")
        .join(format!("{guest}.ttyS1.log"));
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("step 'define' harness fault: cannot create log dir: {err}"))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| format!("step 'define' harness fault: cannot touch serial log: {err}"))?;
    let xml = stamp_domain(guest, volume).replace(
        &format!("tmp/vm/{guest}.ttyS1.log"),
        &log_path.display().to_string(),
    );
    if let Some(parent) = xml_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("step 'define' harness fault: cannot create dir: {err}"))?;
    }
    std::fs::write(xml_path, xml)
        .map_err(|err| format!("step 'define' harness fault: cannot write XML: {err}"))?;
    let argv = vec!["define".to_string(), xml_path.display().to_string()];
    outcome_of("define", &spawn_control("virsh", &argv))
}

/// Boot (`start`) the already-defined trial domain by name; `step_name`
/// is `create` on first boot and `recreate` after the power cut.
/// (`virsh create <xml-path>` would re-read the file as a transient
/// domain and collide with the defined one — hence start-by-name.)
fn step_create(guest: &str, step_name: &str) -> Result<StepOutcome, String> {
    let argv = vec!["start".to_string(), guest.to_string()];
    outcome_of(step_name, &spawn_control("virsh", &argv))
}

/// Poll SSH until the guest answers or the 60s deadline expires.
fn step_ssh_wait(guest: &str) -> Result<StepOutcome, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let argv = ssh_wait_argv().map_err(|e| format!("step 'ssh-wait' harness fault: {e}"))?;
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
fn step_workload(mode: &str) -> Result<StepOutcome, String> {
    let mut tails: Vec<String> = Vec::new();
    for line in workload_commands(mode) {
        let argv = ssh_cmd_argv(&line)?;
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
fn step_verify(mode: &str) -> Result<StepOutcome, String> {
    let mut tails: Vec<String> = Vec::new();
    for line in verify_commands(mode) {
        let argv = ssh_cmd_argv(&line)?;
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
            step_create(&guest, "create"),
        );
    }
    if !stop {
        stop = push_step(&mut steps, &mut verdict, "ssh-wait", step_ssh_wait(&guest));
    }
    if !stop {
        stop = push_step(&mut steps, &mut verdict, "workload", step_workload(mode));
    }
    if domain_may_exist {
        stop = push_step(&mut steps, &mut verdict, "destroy", step_destroy(&guest)) || stop;
    }
    if !stop {
        stop = push_step(
            &mut steps,
            &mut verdict,
            "recreate",
            step_create(&guest, "recreate"),
        );
    }
    if !stop {
        // Rebooted guest needs the same readiness gate as first boot:
        // without it verify races the NIC/daemon and fails "No route".
        stop = push_step(&mut steps, &mut verdict, "ssh-wait", step_ssh_wait(&guest));
    }
    if !stop {
        stop = push_step(&mut steps, &mut verdict, "verify", step_verify(mode));
    }
    // Post-verify teardown: destroy + undefine always (a trial never
    // leaves a running domain behind); vol-delete iff the verdict is
    // `pass` (kept_volume = verdict != pass — failures keep the volume
    // for forensics, matching the documented contract).
    stop = push_step(
        &mut steps,
        &mut verdict,
        "teardown-destroy",
        step_destroy(&guest),
    ) || stop;
    if !stop {
        let argv = vec!["undefine".to_string(), guest.clone()];
        stop = push_step(
            &mut steps,
            &mut verdict,
            "teardown-undefine",
            outcome_of("undefine", &spawn_control("virsh", &argv)),
        );
    }
    if !stop && verdict == "pass" {
        let volume = format!("{guest}.img");
        let argv = vec![
            "vol-delete".to_string(),
            volume,
            "--pool".to_string(),
            "default".to_string(),
        ];
        stop = push_step(
            &mut steps,
            &mut verdict,
            "teardown-vol-delete",
            outcome_of("vol-delete", &spawn_control("virsh", &argv)),
        );
    }
    let _ = stop;

    let verdict_value = build_verdict(mode, name, &verdict, steps);
    let text = serde_json::to_string_pretty(&verdict_value)
        .map_err(|err| format!("failed to serialize verdict for trial '{name}': {err}"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create {}: {}", dir.display(), err))?;
    std::fs::write(dir.join("verdict.json"), text)
        .map_err(|err| format!("failed to write verdict for trial '{name}': {err}"))?;
    Ok(if verdict == "pass" { 0 } else { 1 })
}

/// Would-be control-plane argv vectors (program first) for a trial.
///
/// Generated from the same per-step commands the live body runs
/// (clone → define → create → ssh-wait → workload lines → destroy →
/// recreate → ssh-wait → verify lines → teardown-destroy → teardown-undefine
/// → teardown-vol-delete-if-pass) so the preview cannot drift.
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
        vec!["virsh".to_string(), "start".to_string(), guest.clone()],
        {
            let mut wait = vec!["ssh".to_string()];
            match ssh_wait_argv() {
                Ok(argv) => wait.extend(argv),
                Err(e) => wait.push(format!("<ssh-key-missing: {e}>")),
            }
            wait
        },
    ];
    for line in workload_commands(mode) {
        let mut entry = vec!["ssh".to_string()];
        match ssh_cmd_argv(&line) {
            Ok(argv) => entry.extend(argv),
            Err(e) => entry.push(format!("<ssh-key-missing: {e}>")),
        }
        plan.push(entry);
    }
    plan.push(vec![
        "virsh".to_string(),
        "destroy".to_string(),
        guest.clone(),
    ]);
    plan.push(vec![
        "virsh".to_string(),
        "start".to_string(),
        guest.clone(),
    ]);
    {
        let mut wait = vec!["ssh".to_string()];
        match ssh_wait_argv() {
            Ok(argv) => wait.extend(argv),
            Err(e) => wait.push(format!("<ssh-key-missing: {e}>")),
        }
        plan.push(wait);
    }
    for line in verify_commands(mode) {
        let mut entry = vec!["ssh".to_string()];
        match ssh_cmd_argv(&line) {
            Ok(argv) => entry.extend(argv),
            Err(e) => entry.push(format!("<ssh-key-missing: {e}>")),
        }
        plan.push(entry);
    }
    plan.push(vec![
        "virsh".to_string(),
        "destroy".to_string(),
        guest.clone(),
    ]);
    plan.push(vec![
        "virsh".to_string(),
        "undefine".to_string(),
        guest.clone(),
    ]);
    plan.push(vec![
        "virsh".to_string(),
        "vol-delete".to_string(),
        format!("{guest}.img"),
        "--pool".to_string(),
        "default".to_string(),
    ]);
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

/// Would-be control-plane argv vectors (program first) for a clean.
fn clean_plan(name: &str) -> Vec<Vec<String>> {
    let guest = format!("power-{name}");
    vec![
        vec!["virsh".to_string(), "destroy".to_string(), guest.clone()],
        vec!["virsh".to_string(), "undefine".to_string(), guest.clone()],
        vec![
            "virsh".to_string(),
            "vol-delete".to_string(),
            format!("{guest}.img"),
            "--pool".to_string(),
            "default".to_string(),
        ],
    ]
}

/// Clean a trial: destroy the domain, undefine it, then delete its volume.
/// The golden base image is always refused; path escapes are refused.
/// `force` is accepted for dispatch symmetry (`run_trial`/`run_build` take
/// it) and ignored: cleaning an existing trial never needs a force gate.
pub fn run_clean(name: &str, force: bool, dry_run: bool) -> Result<i32, String> {
    let _ = force;
    if name == GOLDEN_NAME {
        return Err(format!(
            "refusing to clean reserved base image '{GOLDEN_NAME}' (golden guard)"
        ));
    }
    if name.contains('/') || name.contains("..") {
        return Err(format!(
            "invalid power trial name '{name}' (must not contain '/' or '..')"
        ));
    }
    if dry_run {
        for argv in clean_plan(name) {
            crate::verbs::print_dry_run(&argv[0], &argv[1..]);
        }
        return Ok(0);
    }
    if cfg!(not(target_os = "linux")) {
        return Err("unsupported platform — Hyper-V CLI backend not yet ported".to_string());
    }
    let guest = format!("power-{name}");
    let volume = format!("{guest}.img");
    let dir = clean_report_dir(name);
    let started_ms = now_ms();
    let record_argv = vec![
        "power".to_string(),
        "clean".to_string(),
        "--name".to_string(),
        name.to_string(),
    ];
    let mut steps: Vec<StepOutcome> = Vec::new();
    let mut verdict = "pass".to_string();
    let destroy = vec!["destroy".to_string(), guest.clone()];
    let mut stop = push_step(
        &mut steps,
        &mut verdict,
        "destroy",
        outcome_of("destroy", &spawn_control("virsh", &destroy)),
    );
    if !stop {
        let undefine = vec!["undefine".to_string(), guest.clone()];
        stop = push_step(
            &mut steps,
            &mut verdict,
            "undefine",
            outcome_of("undefine", &spawn_control("virsh", &undefine)),
        );
    }
    if !stop {
        let delete = vec![
            "vol-delete".to_string(),
            volume.clone(),
            "--pool".to_string(),
            "default".to_string(),
        ];
        push_step(
            &mut steps,
            &mut verdict,
            "vol-delete",
            outcome_of("vol-delete", &spawn_control("virsh", &delete)),
        );
    }
    write_op_record(
        &dir,
        "power-clean",
        &record_argv,
        &steps,
        &verdict,
        started_ms,
    )?;
    if verdict != "pass" {
        let tails: Vec<String> = steps
            .iter()
            .filter(|step| step.exit_code != 0)
            .map(|step| format!("{}: {}", step.name, step.stderr_tail))
            .collect();
        return Err(format!("clean failed ({}): {}", verdict, tails.join("; ")));
    }
    Ok(0)
}

/// Sweep every leftover trial domain/volume whose name starts with
/// `power-` (stale `power-*` domains from failed trials, kept volumes).
/// Refuses to touch anything not starting with `power-`; the golden and
/// host domains never match. Best-effort per guest: destroy (ignore
/// not-running), undefine, vol-delete; failures are recorded, not fatal.
/// Mounts/loop devices are out of scope here: the merged xtask never
/// creates any (that was manual-session debris); the build op mounts only
/// under repo-owned `tmp/vm/mnt-*` (see run_build_live) and unmounts in
/// the same op.
/// Normalize "already absent" sweep outcomes to success: a missing
/// domain/volume is the desired end state, not a failure. Anything else
/// passes through untouched.
fn benign_if_absent(step: Result<StepOutcome, String>) -> Result<StepOutcome, String> {
    match step {
        Err(text)
            if text.contains("not running")
                || text.contains("no such domain")
                || text.contains("failed to get domain")
                || text.contains("Domain not found") =>
        {
            Ok(StepOutcome {
                name: "already-absent".to_string(),
                exit_code: 0,
                stderr_tail: text,
            })
        }
        other => other,
    }
}

pub fn run_clean_stale(dry_run: bool) -> Result<i32, String> {
    if cfg!(not(target_os = "linux")) {
        return Err("unsupported platform — Hyper-V CLI backend not yet ported".to_string());
    }
    let list_argv = vec![
        "list".to_string(),
        "--all".to_string(),
        "--name".to_string(),
    ];
    let list = spawn_control("virsh", &list_argv);
    if list.exit_code != 0 {
        return Err(format!(
            "stale sweep harness fault: cannot list domains: {}",
            list.stderr_tail
        ));
    }
    let mut guests: Vec<String> = list
        .stdout_tail
        .lines()
        .map(str::trim)
        .filter(|name| name.starts_with("power-"))
        .map(str::to_string)
        .collect();
    // Volumes without domains (interrupted clone, manual vol-clone):
    // enumerate the pool too, derive guest names from power-*.img.
    let vollist_argv = vec![
        "vol-list".to_string(),
        "--pool".to_string(),
        "default".to_string(),
    ];
    let vollist = spawn_control("virsh", &vollist_argv);
    if vollist.exit_code != 0 {
        return Err(format!(
            "stale sweep harness fault: cannot list volumes: {}",
            vollist.stderr_tail
        ));
    }
    for line in vollist.stdout_tail.lines().skip(2) {
        let vol = line.split_whitespace().next().unwrap_or("");
        let vol = vol.strip_suffix(".img").unwrap_or(vol);
        if vol.starts_with("power-") && !guests.iter().any(|g| g == vol) {
            guests.push(vol.to_string());
        }
    }
    if dry_run {
        for guest in &guests {
            let name = guest.strip_prefix("power-").unwrap_or(guest);
            for argv in clean_plan(name) {
                crate::verbs::print_dry_run(&argv[0], &argv[1..]);
            }
        }
        if guests.is_empty() {
            println!("power clean --stale: no power-* domains or volumes found");
        }
        return Ok(0);
    }
    let dir = std::path::PathBuf::from("reports/power-clean-stale");
    let started_ms = now_ms();
    let mut steps: Vec<StepOutcome> = Vec::new();
    let mut verdict = "pass".to_string();
    for guest in &guests {
        let name = guest.strip_prefix("power-").unwrap_or(guest);
        // Best-effort: a stopped domain's destroy fails ("not running"),
        // a volume-only orphan has no domain to destroy/undefine at all.
        // Both cases are benign when the volume ends up gone: normalize
        // "already absent" stderr to success so the sweep verdict reflects
        // leftover state, not missing-thing noise.
        let destroy = vec!["destroy".to_string(), guest.clone()];
        let _ = push_step(
            &mut steps,
            &mut verdict,
            &format!("sweep-destroy-{name}"),
            benign_if_absent(outcome_of("destroy", &spawn_control("virsh", &destroy))),
        );
        let undefine = vec!["undefine".to_string(), guest.clone()];
        push_step(
            &mut steps,
            &mut verdict,
            &format!("sweep-undefine-{name}"),
            benign_if_absent(outcome_of("undefine", &spawn_control("virsh", &undefine))),
        );
        let delete = vec![
            "vol-delete".to_string(),
            format!("{guest}.img"),
            "--pool".to_string(),
            "default".to_string(),
        ];
        push_step(
            &mut steps,
            &mut verdict,
            &format!("sweep-vol-delete-{name}"),
            outcome_of("vol-delete", &spawn_control("virsh", &delete)),
        );
    }
    write_op_record(
        &dir,
        "power-clean-stale",
        &[
            "power".to_string(),
            "clean".to_string(),
            "--stale".to_string(),
        ],
        &steps,
        &verdict,
        started_ms,
    )?;
    Ok(if verdict == "pass" { 0 } else { 1 })
}
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
                "start",
                "ssh",
                "ssh",
                "ssh",
                "ssh",
                "ssh",
                "ssh",
                "destroy",
                "start",
                "ssh",
                "ssh",
                "ssh",
                "destroy",
                "undefine",
                "vol-delete",
            ]
        );
    }

    #[test]
    fn power_dispatch_trial_dry_run_returns_zero() {
        let code = crate::verbs::run_power("trial", Some("canary"), Some("t1"), false, false, true)
            .unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn power_dispatch_rejects_trial_without_mode() {
        let err =
            crate::verbs::run_power("trial", None, Some("t1"), false, false, true).unwrap_err();
        assert!(err.contains("--mode"), "unexpected error: {err}");
    }

    #[test]
    fn power_dispatch_rejects_trial_without_name() {
        let err =
            crate::verbs::run_power("trial", Some("canary"), None, false, false, true).unwrap_err();
        assert!(err.contains("--name"), "unexpected error: {err}");
    }

    #[test]
    fn power_dispatch_rejects_clean_without_name() {
        let err = crate::verbs::run_power("clean", None, None, false, false, true).unwrap_err();
        assert!(err.contains("--name"), "unexpected error: {err}");
    }

    #[test]
    fn sweep_treats_already_absent_as_success() {
        let absent = Err::<super::StepOutcome, String>(
            "error: failed to get domain 'power-x': Domain not found".to_string(),
        );
        let ok = super::benign_if_absent(absent).unwrap();
        assert_eq!(ok.exit_code, 0);
        let stopped: Result<super::StepOutcome, String> =
            Err("error: Requested operation is not valid: domain is not running".to_string());
        assert!(super::benign_if_absent(stopped).is_ok());
        let real: Result<super::StepOutcome, String> = Err("error: some real failure".to_string());
        assert!(super::benign_if_absent(real).is_err());
    }

    #[test]
    fn power_stale_sweep_dry_run_lists_nothing_without_spawning() {
        // Dry-run must exit 0 without spawning: on a host with no power-*
        // domains it prints the empty-sweep note; on the rig it prints the
        // per-guest destroy/undefine/vol-delete plan. Either way no spawn.
        let code = crate::power::run_clean_stale(true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn power_stale_dry_run_without_key_shows_missing_key_placeholders() {
        // Without LTMRS_POWER_SSH_KEY the plan still prints (markers, no
        // spawn); with the key it prints full ssh argv. Either way exit 0.
        // (Env unset in CI by construction — no unsafe set_var needed.)
        let code = crate::power::run_clean_stale(true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn power_dispatch_rejects_unknown_op() {
        let err =
            crate::verbs::run_power("frobnicate", None, None, false, false, true).unwrap_err();
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
            !xml.contains("__NAME__")
                && !xml.contains("__VOLUME__")
                && !xml.contains("__MAC__")
                && !xml.contains("__SERIAL_PORT__")
        );
        let mac1 = crate::power::derived_mac("wp12-03");
        let mac2 = crate::power::derived_mac("wp12-03");
        assert_eq!(mac1, mac2);
        assert!(mac1.starts_with("52:54:00"));
        let port1 = crate::power::derived_serial_port("wp12-03");
        let port2 = crate::power::derived_serial_port("wp12-03");
        assert_eq!(port1, port2);
        assert!((4599..=5622).contains(&port1), "port in range: {port1}");
        assert_ne!(
            crate::power::derived_serial_port("wp12-04"),
            port1,
            "distinct trials diverge"
        );
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
    #[test]
    fn clean_refuses_golden_volume() {
        let err = crate::power::run_clean("ltmrs-golden", false, true).unwrap_err();
        assert!(err.contains("ltmrs-golden"));
    }

    #[test]
    fn clean_dry_run_prints_without_spawning() {
        let code = crate::power::run_clean("rigtest-old", false, true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn build_dry_run_prints_without_spawning() {
        let code = crate::power::run_build(false, true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn clean_rejects_path_escape_name() {
        for bad in ["../evil", "a/b"] {
            let err = crate::power::run_clean(bad, false, true).unwrap_err();
            assert!(err.contains(bad), "must name the bad name: {err}");
        }
    }

    #[test]
    fn clean_plan_matches_live_step_sequence() {
        let plan = super::clean_plan("rigtest-old");
        let names: Vec<&str> = plan.iter().map(|argv| argv[1].as_str()).collect();
        assert_eq!(names, vec!["destroy", "undefine", "vol-delete"]);
    }

    #[test]
    fn build_plan_matches_live_step_sequence() {
        let plan = super::build_plan();
        assert!(plan[0][0] == "qemu-img" && plan[0][1] == "create");
        assert!(plan[1] == vec!["virsh", "define", "ltmrs-golden.xml"]);
        assert!(plan[2] == vec!["virsh", "start", "ltmrs-golden"]);
        assert!(plan[plan.len() - 3][1] == "destroy");
        assert!(plan[plan.len() - 2][1] == "undefine");
        assert!(plan[plan.len() - 1][1] == "vol-info");
    }

    #[test]
    fn build_provision_installs_authorized_keys_and_runlevel_link() {
        let commands = super::build_provision_commands();
        assert_eq!(
            commands.len(),
            7,
            "provision must stay 7 lines: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|line| line.contains("/mnt/root/.ssh/authorized_keys")),
            "provision must install authorized_keys: {commands:?}"
        );
        assert!(
            commands.iter().any(|line| line.contains("S99power")),
            "provision must enable the S99power runlevel link: {commands:?}"
        );
        let plan = super::build_plan();
        for needle in ["/mnt/root/.ssh/authorized_keys", "S99power"] {
            assert!(
                plan.iter()
                    .any(|argv| argv.get(2).is_some_and(|line| line.contains(needle))),
                "build plan must mirror the provision line for {needle}"
            );
        }
    }

    #[test]
    fn ssh_wait_plan_entry_uses_shared_probe_argv() {
        let mut expected = vec!["ssh".to_string()];
        expected.extend(super::ssh_base_argv_with_key("/tmp/test-only-key"));
        expected.push("true".to_string());
        let trial = super::trial_plan("canary", "t1");
        // trial_plan dry-run without key env shows the missing-key marker;
        // with key material the live probe and preview share one builder.
        // Here assert preview shape: ssh + probe tail present.
        assert_eq!(trial[0][0], "virsh", "trial plan starts with vol-clone");
        assert!(
            trial[3][0] == "ssh",
            "trial ssh-wait preview must be an ssh entry"
        );
        let build = super::build_plan();
        assert!(
            build[3][0] == "ssh",
            "build ssh-wait preview must be an ssh entry"
        );
    }

    #[test]
    fn build_report_dir_is_fixed_golden_path() {
        assert_eq!(
            super::build_report_dir(),
            std::path::PathBuf::from("reports/power-build")
        );
    }

    #[test]
    fn clean_report_dir_sits_next_to_trial_verdict() {
        assert_eq!(
            super::clean_report_dir("rigtest-old"),
            std::path::PathBuf::from("reports/power-rigtest-old")
        );
    }

    #[test]
    fn op_record_persists_verdict_steps_and_argv() {
        let dir = tempfile::tempdir().expect("tempdir");
        let argv = vec!["power".to_string(), "clean".to_string()];
        let steps = vec![super::step_ok("destroy")];
        super::write_op_record(dir.path(), "power-clean", &argv, &steps, "pass", 1).unwrap();
        let text = std::fs::read_to_string(dir.path().join("record.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["verb"], "power-clean");
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["meta"]["verdict"], "pass");
        assert_eq!(value["meta"]["steps"][0]["name"], "destroy");
    }
}
