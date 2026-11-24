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

#[cfg(unix)]
#[test]
fn project_factories_vendor_shared_modules_and_project_specialists() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_modules.py");
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
fn a_captain_run_shows_in_the_native_factory_overview_and_stop_resume_work() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_overview.py");
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
fn captain_turns_run_on_the_daemon_open_live_and_keep_their_actor() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_live.py");
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
fn factory_models_reach_the_design_turn_and_a_default_change_needs_approval() {
    let temp = tempfile::tempdir().unwrap();
    let test =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_factory_models.py");
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
fn captain_work_survives_crashes_missing_worktrees_and_waits_for_user_gates() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_durability.py");
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
fn captain_decisions_budgets_schedules_stacks_checkpoints_and_findings() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_wave2.py");
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
fn only_the_user_stops_a_captain_and_sessions_keep_their_own_actor() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_actors.py");
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
fn an_archived_captain_frees_its_root_and_unarchives_when_the_root_is_free() {
    let temp = tempfile::tempdir().unwrap();
    let test = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/captain_archive.py");
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
