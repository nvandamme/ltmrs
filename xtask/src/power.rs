//! Power rig as code: build the base image, run a trial, or clean a trial.
//!
//! Task 2 owns the runner surface (dispatch validation, dry-run plan,
//! golden guard). The live trial/build/clean bodies land in Tasks 4-5.

/// Reserved base-image name: trials clone FROM it, never run AS it.
const GOLDEN_NAME: &str = "ltmrs-golden";

/// Build the base image (stub: Task 5 implements).
pub fn run_build(_force: bool, _dry_run: bool) -> Result<i32, String> {
    Err("not implemented".to_string())
}

/// Would-be control-plane argv vectors (program first) for a trial.
fn trial_plan(mode: &str, name: &str) -> Vec<Vec<String>> {
    let guest = format!("power-{name}");
    vec![
        vec![
            "virsh".to_string(),
            "vol-clone".to_string(),
            format!("{GOLDEN_NAME}-vol"),
            format!("{guest}.img"),
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
        vec!["ssh".to_string(), guest.clone(), format!("trial-{mode}")],
        vec!["virsh".to_string(), "destroy".to_string(), guest],
    ]
}

/// Run a trial (Task 2: dry-run plan only; the live path lands in Task 4).
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
    if !dry_run {
        return Err("not implemented".to_string());
    }
    for argv in trial_plan(mode, name) {
        crate::verbs::print_dry_run(&argv[0], &argv[1..]);
    }
    Ok(0)
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
}
