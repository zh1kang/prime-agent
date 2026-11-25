//! `/connect claude|codex|antigravity` inside pa: the provider's login program runs with piped
//! stdio (through `pa thread connect`, which refuses when the provider is already signed in),
//! and the inline auth panel shows its sign-in link, codes, and progress. A code the program
//! asks for is typed into the panel's field and written to the program's stdin. The terminal
//! never leaves pa.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::auth_panel::{AuthPanelHandle, PastePromptTone, PasteStyle};

/// One line of the login program's output, sorted for the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoginLine {
    /// A sign-in link (never a local callback address).
    Url(String),
    /// The program waits for a pasted code.
    Prompt(String),
    Text(String),
}

/// Sort one output line. `partial` marks text without a final newline: only a prompt shape
/// counts there, because the rest of the line may still arrive.
pub(crate) fn classify(line: &str, partial: bool) -> Option<LoginLine> {
    let line = crate::ansi::strip_ansi(line);
    let line = crate::menu_panel::scrub_controls(&line);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let lower = line.to_lowercase();
    let prompt = line.ends_with(':') || line.ends_with('?') || line.ends_with('>');
    if prompt && (lower.contains("paste") || lower.contains("enter") || lower.contains("code")) {
        return Some(LoginLine::Prompt(line.to_owned()));
    }
    if partial {
        return None;
    }
    if let Some(url) = line
        .split_whitespace()
        .find(|w| w.starts_with("https://") || w.starts_with("http://"))
    {
        let url = url.trim_end_matches(['.', ',', ')']);
        let local = url.contains("://localhost") || url.contains("://127.0.0.1");
        if !local {
            return Some(LoginLine::Url(url.to_owned()));
        }
    }
    Some(LoginLine::Text(line.to_owned()))
}

/// Run one login command to its end with the panel as its only surface. Returns the note for
/// the transcript.
pub(crate) async fn run(
    mut command: tokio::process::Command,
    provider: &str,
    panel: AuthPanelHandle,
) -> String {
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return format!("Could not start the {provider} sign-in: {error}"),
    };
    let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return format!("Could not start the {provider} sign-in.");
    };
    panel.progress_line(format!("Starting the {provider} sign-in…"));
    let mut buffer = String::new();
    let mut last = String::new();
    let mut shown_url = false;
    let mut out = [0u8; 4096];
    let mut err = [0u8; 4096];
    let mut out_open = true;
    let mut err_open = true;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    while out_open || err_open {
        let chunk = tokio::select! {
            read = stdout.read(&mut out), if out_open => match read {
                Ok(0) | Err(_) => { out_open = false; None }
                Ok(n) => Some(String::from_utf8_lossy(&out[..n]).into_owned()),
            },
            read = stderr.read(&mut err), if err_open => match read {
                Ok(0) | Err(_) => { err_open = false; None }
                Ok(n) => Some(String::from_utf8_lossy(&err[..n]).into_owned()),
            },
            _ = tick.tick() => {
                if panel.cancelled() {
                    let _ = child.kill().await;
                    return format!("{provider} sign-in cancelled.");
                }
                None
            }
        };
        let Some(chunk) = chunk else { continue };
        buffer.push_str(&chunk.replace('\r', "\n"));
        let mut lines: Vec<(String, bool)> = Vec::new();
        while let Some(end) = buffer.find('\n') {
            lines.push((buffer[..end].to_owned(), false));
            buffer.drain(..=end);
        }
        if matches!(classify(&buffer, true), Some(LoginLine::Prompt(_))) {
            lines.push((std::mem::take(&mut buffer), false));
        }
        for (line, partial) in lines {
            match classify(&line, partial) {
                None => {}
                Some(LoginLine::Url(url)) if !shown_url => {
                    shown_url = true;
                    panel.auth_url(&url, Some("Open this link to sign in."));
                }
                Some(LoginLine::Url(url)) => panel.progress_line(url),
                Some(LoginLine::Prompt(prompt)) => {
                    let answer = panel
                        .paste_prompt(&prompt, PastePromptTone::Text, PasteStyle::Visible)
                        .await;
                    let Some(answer) = answer else {
                        let _ = child.kill().await;
                        return format!("{provider} sign-in cancelled.");
                    };
                    let line = format!("{}\n", answer.trim());
                    if stdin.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
                Some(LoginLine::Text(text)) => {
                    last.clone_from(&text);
                    panel.progress_line(text);
                }
            }
        }
    }
    drop(stdin);
    match child.wait().await {
        Ok(status) if status.success() => format!("Signed in to {provider}."),
        Ok(_) | Err(_) => {
            let reason = last.trim().trim_start_matches("Error: ").to_owned();
            if reason.is_empty() {
                format!("{provider} sign-in failed.")
            } else {
                reason
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_lines_sort_into_links_prompts_and_text() {
        assert_eq!(
            classify("Go to https://auth.example.test/device?x=1.", false),
            Some(LoginLine::Url(
                "https://auth.example.test/device?x=1".into()
            ))
        );
        assert_eq!(
            classify("Starting server on http://localhost:1455", false),
            Some(LoginLine::Text(
                "Starting server on http://localhost:1455".into()
            ))
        );
        assert_eq!(
            classify(
                "\u{1b}[1mPaste the authorization code here:\u{1b}[0m ",
                true
            ),
            Some(LoginLine::Prompt(
                "Paste the authorization code here:".into()
            ))
        );
        assert_eq!(classify("Enter this one-time code", true), None);
        assert_eq!(classify("   ", false), None);
    }

    /// A fake login program: it prints a link and a prompt, reads the pasted code from stdin,
    /// and exits with success only for that code.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_login_program_gets_the_pasted_code_and_the_panel_gets_its_link() {
        let script = "printf 'Open https://auth.example.test/a\\nPaste the code here: '; \
                      read code; [ \"$code\" = ABC-123 ] && echo done";
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", script]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let panel = AuthPanelHandle::new(tx);
        let flow = tokio::spawn(run(command, "fake", panel));
        let mut url = None;
        while let Some(request) = rx.recv().await {
            match request {
                crate::auth_panel::AuthPanelRequest::AuthUrl { url: u, .. } => url = Some(u),
                crate::auth_panel::AuthPanelRequest::PastePrompt { prompt, reply, .. } => {
                    assert_eq!(prompt, "Paste the code here:");
                    reply.send(Some("ABC-123".into())).unwrap();
                }
                _ => {}
            }
            if flow.is_finished() {
                break;
            }
        }
        assert_eq!(flow.await.unwrap(), "Signed in to fake.");
        assert_eq!(url.as_deref(), Some("https://auth.example.test/a"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_login_reports_the_program_reason() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "echo 'Error: codex is already signed in. Run codex logout first.' >&2; exit 1",
        ]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let note = run(command, "codex", AuthPanelHandle::new(tx)).await;
        assert_eq!(note, "codex is already signed in. Run codex logout first.");
    }
}
