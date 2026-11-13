//! Power rig as code: build the base image, run a trial, or clean a trial.
//!
//! Task 1 holds stubs only; Task 2 fills the runner bodies.

/// Build the base image (stub: Task 2 implements).
pub fn run_build(_force: bool, _dry_run: bool) -> Result<i32, String> {
    Err("not implemented".to_string())
}

/// Run a trial (stub: Task 2 implements).
pub fn run_trial(_mode: &str, _name: &str, _force: bool, _dry_run: bool) -> Result<i32, String> {
    Err("not implemented".to_string())
}

/// Clean a trial (stub: Task 2 implements).
pub fn run_clean(_name: &str, _force: bool, _dry_run: bool) -> Result<i32, String> {
    Err("not implemented".to_string())
}
