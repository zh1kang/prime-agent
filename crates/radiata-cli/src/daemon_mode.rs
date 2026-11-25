//! `--mode daemon`: the supervisor process. The interactive client spawns this
//! mode (detached) when no daemon is listening.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config;

pub fn run_daemon_mode(daemon_socket: Option<&str>) -> Result<i32> {
    let socket_path = config::resolve_daemon_socket_path(daemon_socket);
    let agent_dir = config::get_agent_dir();
    spawn_captain_reconcile(&socket_path);
    let options = radiata_daemon::supervisor::SupervisorOptions {
        socket_path,
        agent_dir,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(radiata_daemon::supervisor::run_supervisor(options))?;
    Ok(0)
}

/// The Captain store, resolved as `captain_backend.py` resolves it.
fn captain_store() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let defaults: Vec<PathBuf> = home
        .iter()
        .flat_map(|home| [home.join(".prime/harness"), home.join(".prime/agent")])
        .map(|dir| dir.canonicalize().unwrap_or(dir))
        .collect();
    let custom = std::env::var_os("PRIME_AGENT_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .filter(|dir| !defaults.contains(&dir.canonicalize().unwrap_or_else(|_| dir.clone())));
    let hub = std::env::var_os("PA_CAPTAIN_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or(custom)
        .or_else(|| home.map(|home| home.join(".prime/harness")))?;
    Some(hub.join("captains/projects.sqlite3"))
}

/// Reconcile Captain jobs a previous daemon left behind, once this daemon serves. Only a
/// machine with a Captain store runs it, and its outcome never affects the daemon.
fn spawn_captain_reconcile(socket: &Path) {
    if !captain_store().is_some_and(|store| store.is_file()) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let socket = socket.to_path_buf();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !socket.exists() {
            if Instant::now() > deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = Command::new(exe)
            .args(["thread", "captain", "reconcile"])
            .env("PRIME_AGENT_DAEMON_SOCKET", &socket)
            .env_remove("PA_CAPTAIN_ACTOR")
            .env_remove("PA_CAPTAIN_SESSION")
            .env_remove("PA_CAPTAIN_SELECTION")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}
