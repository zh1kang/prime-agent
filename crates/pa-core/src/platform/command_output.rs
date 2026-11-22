//! Bounded child-process output: one command run to completion under a deadline, with each
//! output stream capped. The workspace git operations and the external-agent probes share it:
//! both run third-party executables (git hooks and filters, vendor CLIs) that can hang or
//! flood their pipes.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Interval between exit polls while a bounded command runs.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// The captured result of one bounded command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedOutput {
    /// The exit code; `None` when a signal ended the process or the deadline killed it.
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The deadline passed and the process was killed.
    pub timed_out: bool,
    /// At least one stream exceeded `max_stream_bytes`; the kept prefix is in the buffer.
    pub truncated: bool,
}

impl BoundedOutput {
    /// The command exited with code 0 inside its deadline.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.status == Some(0) && !self.timed_out
    }

    /// Stdout as lossy UTF-8.
    #[must_use]
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Stderr as lossy UTF-8, trimmed.
    #[must_use]
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

fn read_capped(mut stream: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = cap.saturating_sub(kept.len());
                if read > room {
                    truncated = true;
                }
                kept.extend_from_slice(&buffer[..read.min(room)]);
            }
        }
    }
    (kept, truncated)
}

/// Run `command` with null stdin and piped output until it exits or `timeout` passes. The
/// readers keep draining past the cap so a chatty child never blocks on a full pipe.
///
/// # Errors
///
/// The spawn error when the program cannot start (a missing executable reports
/// `ErrorKind::NotFound`).
pub fn output_with_timeout(
    command: &mut Command,
    timeout: Duration,
    max_stream_bytes: usize,
) -> std::io::Result<BoundedOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    super::set_no_window(command);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader =
        std::thread::spawn(move || stdout.map(|s| read_capped(s, max_stream_bytes)));
    let stderr_reader =
        std::thread::spawn(move || stderr.map(|s| read_capped(s, max_stream_bytes)));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status.code();
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(EXIT_POLL_INTERVAL);
    };
    let (stdout, stdout_truncated) = stdout_reader.join().ok().flatten().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_reader.join().ok().flatten().unwrap_or_default();
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_hanging_command_is_killed_at_the_deadline() {
        let started = Instant::now();
        let output = output_with_timeout(
            Command::new("sleep").arg("30"),
            Duration::from_millis(200),
            64,
        )
        .expect("spawn sleep");
        assert!(output.timed_out);
        assert_eq!(output.status, None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn streams_past_the_cap_are_truncated_without_blocking() {
        let output = output_with_timeout(
            Command::new("sh").args(["-c", "yes x | head -c 100000; echo err >&2"]),
            Duration::from_secs(20),
            16,
        )
        .expect("spawn sh");
        assert_eq!(
            output,
            BoundedOutput {
                status: Some(0),
                stdout: b"x\nx\nx\nx\nx\nx\nx\nx\n".to_vec(),
                stderr: b"err\n".to_vec(),
                timed_out: false,
                truncated: true,
            }
        );
    }
}
