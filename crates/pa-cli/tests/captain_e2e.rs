//! Exercise Captain restart recovery with two local projects and scripted providers.
#[cfg(unix)]
#[test]
fn captain_reconciles_an_interrupted_dispatch_and_keeps_worktrees_separate() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_recovery.py");
    let output = std::process::Command::new("python3")
        .arg(test)
        .arg(env!("CARGO_BIN_EXE_prime-agent"))
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn captain_writes_its_rules_and_supports_promote_and_demote() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_onboarding.py");
    let output = std::process::Command::new("python3")
        .arg(test)
        .arg(env!("CARGO_BIN_EXE_prime-agent"))
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn captain_runs_factory_machines_headless_with_one_dispatch_path() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_machines.py");
    let output = std::process::Command::new("python3")
        .arg(test)
        .arg(env!("CARGO_BIN_EXE_prime-agent"))
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn captain_stop_resume_and_delete_settle_running_work() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_states.py");
    let output = std::process::Command::new("python3")
        .arg(test)
        .arg(env!("CARGO_BIN_EXE_prime-agent"))
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn a_project_factory_edit_in_the_machine_editor_needs_approval_again() {
    let temp = tempfile::tempdir().unwrap();
    let test =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_factory_edit.py");
    let output = std::process::Command::new("python3")
        .arg(test)
        .arg(env!("CARGO_BIN_EXE_prime-agent"))
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
