//! The local CLI model listing probes (`agy models`, `cursor-agent --list-models`): a bounded
//! run and a cross-process disk cache of the raw listing, so a fresh `pa` process reuses a recent
//! listing instead of paying the CLI's start-up cost on every launch.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// How long a routine listing probe may run before it is killed.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Longer bound for explicit/forced probes (e.g. cold CLI startup on `/login` or manual refresh).
pub(crate) const FORCE_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum time to wait for pipe reader threads after the child exits, in case a grandchild
/// inherited stdout/stderr and kept the pipe open.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// The captured result of a finished probe.
pub(crate) struct ProbeOutput {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Run `bin args` with stdin closed and return its output, or `None` when it could not start or
/// did not finish within `timeout` (the child is then killed). Both pipes drain on their own
/// threads, so a listing larger than the pipe buffer cannot stall the child until the deadline.
pub(crate) fn run_bounded(bin: &Path, args: &[&str], timeout: Duration) -> Option<ProbeOutput> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    run_command_bounded(cmd, timeout)
}

/// Run a pre-configured `Command` with stdin closed and a hard wall-clock bound.
pub(crate) fn run_command_bounded(mut cmd: Command, timeout: Duration) -> Option<ProbeOutput> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let status = status?;
    // A grandchild that inherited stdout/stderr could keep a pipe open even after the child
    // exits; wait only a bounded grace window so probes can never hang indefinitely.
    Some(ProbeOutput {
        success: status.success(),
        stdout: stdout.recv_timeout(DRAIN_GRACE).unwrap_or_default(),
        stderr: stderr.recv_timeout(DRAIN_GRACE).unwrap_or_default(),
    })
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    rx
}

/// `<agent dir>/cache/<name>`, the shared cache file of one probe.
pub(crate) fn cache_path(name: &str) -> Option<PathBuf> {
    pa_types::platform::dirs::agent_dir().map(|dir| dir.join("cache").join(name))
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// The cached listing for `bin` and its age, when it is younger than `ttl`. A listing from a
/// different binary, a future timestamp, or an unreadable file is a miss.
pub(crate) fn read_cached(path: &Path, bin: &Path, ttl: Duration) -> Option<(String, Duration)> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if doc.get("bin")?.as_str()? != bin.to_string_lossy() {
        return None;
    }
    let age = unix_now() - doc.get("fetched_at")?.as_f64()?;
    if !(0.0..ttl.as_secs_f64()).contains(&age) {
        return None;
    }
    let stdout = doc.get("stdout")?.as_str()?.to_owned();
    Some((stdout, Duration::from_secs_f64(age)))
}

/// Store a successful listing through a temporary file and a rename, so a concurrent reader
/// never sees a partial document.
pub(crate) fn write_cached(path: &Path, bin: &Path, stdout: &str) -> std::io::Result<()> {
    let body = json!({
        "fetched_at": unix_now(),
        "bin": bin.to_string_lossy(),
        "stdout": stdout,
    })
    .to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, body)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// The `Instant` a listing of `age` was fetched at, for the in-process cache's TTL check.
pub(crate) fn fetched_instant(age: Duration) -> Instant {
    Instant::now().checked_sub(age).unwrap_or_else(Instant::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trips_for_the_same_binary_within_the_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache").join("probe.json");
        let bin = Path::new("/usr/local/bin/agy");
        write_cached(&path, bin, "a - A\n").unwrap();
        let (stdout, age) = read_cached(&path, bin, Duration::from_secs(300)).unwrap();
        assert_eq!(stdout, "a - A\n");
        assert!(age < Duration::from_secs(5));
        assert!(read_cached(&path, Path::new("/other/agy"), Duration::from_secs(300)).is_none());
        assert!(read_cached(&path, bin, Duration::ZERO).is_none());
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn stale_or_corrupt_cache_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.json");
        let bin = Path::new("/bin/agy");
        std::fs::write(
            &path,
            json!({ "fetched_at": unix_now() - 600.0, "bin": "/bin/agy", "stdout": "x" })
                .to_string(),
        )
        .unwrap();
        assert!(read_cached(&path, bin, Duration::from_secs(300)).is_none());
        std::fs::write(&path, "{").unwrap();
        assert!(read_cached(&path, bin, Duration::from_secs(300)).is_none());
    }

    #[cfg(unix)]
    fn script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("probe");
        std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(unix)]
    #[test]
    fn bounded_run_captures_output_and_kills_a_hung_probe() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(dir.path(), "echo listed; echo note >&2");
        let output = run_bounded(&ok, &[], Duration::from_secs(5)).unwrap();
        assert!(output.success);
        assert_eq!(output.stdout.trim(), "listed");
        assert_eq!(output.stderr.trim(), "note");

        let hung = script(dir.path(), "exec sleep 30");
        let started = Instant::now();
        assert!(run_bounded(&hung, &[], Duration::from_millis(200)).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
