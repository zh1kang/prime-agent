//! Headless e2e for the loader between a submit and the daemon's prompt ACK: the optimistic
//! submit paints the working loader on the Enter frame, and the spinner keeps its 80ms cadence
//! and the elapsed counter advances once per second even while no ACK or stream event arrives.
#![cfg(unix)]

mod support;

use pa_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, UiMode,
};
use serde_json::json;

use support::{
    accept_within_idle_window, attach_response, create_response, options, serve_connection,
    write_json, MockSupervisor, Request,
};

/// How long the mock holds the prompt ACK.
const ACK_DELAY_MS: u64 = 3500;

impl MockSupervisor {
    fn serve(self) {
        let Some(stream) = accept_within_idle_window(&self.listener) else {
            return;
        };
        serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
            match kind {
                "create" => write_json(writer, &create_response(id)),
                "attach" => write_json(writer, &attach_response(id, "ack delay session")),
                "prompt" => {
                    // The ACK and the turn's end arrive late from a side thread: the window
                    // between Enter and the first daemon frame has no event to wake the loop.
                    let mut late = writer.try_clone().expect("clone socket");
                    let id = id.to_owned();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(ACK_DELAY_MS));
                        write_json(
                            &mut late,
                            &json!({ "type": "response", "id": id, "command": "prompt", "success": true }),
                        );
                        for event_type in ["turn_end", "agent_end"] {
                            write_json(
                                &mut late,
                                &json!({
                                    "type": "session_event",
                                    "activeSessionId": "s1",
                                    "event": { "type": event_type },
                                }),
                            );
                        }
                    });
                }
                _ => return false,
            }
            true
        });
    }
}

fn spinner_and_secs(line: &str) -> Option<(char, u64)> {
    let spinner = line.trim_start().chars().next()?;
    let secs = line
        .split('·')
        .nth(1)?
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((spinner, secs))
}

#[test]
fn the_loader_animates_before_the_prompt_ack() {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let supervisor = MockSupervisor::bind(&socket);
    let handle = std::thread::spawn(move || supervisor.serve());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps: vec![
            HeadlessStep::Submit("run the sweep".to_owned()),
            HeadlessStep::WaitMs(3000),
        ],
        width: 100,
        height: 40,
    };
    let outcome = runtime
        .block_on(run_interactive(
            InteractiveOptions {
                fullscreen_mouse: false,
                ..options(socket)
            },
            UiMode::Headless(plan),
        ))
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");

    let loader_lines: Vec<&str> = outcome
        .frames
        .iter()
        .filter_map(|frame| frame.lines().find(|line| line.contains("Waiting")))
        .collect();
    let samples: Vec<(char, u64)> = loader_lines
        .iter()
        .filter_map(|line| spinner_and_secs(line))
        .collect();
    let transcript = loader_lines.join("\n");
    let early: Vec<&(char, u64)> = samples.iter().filter(|(_, secs)| *secs < 3).collect();
    let mut glyphs: Vec<char> = early.iter().map(|(glyph, _)| *glyph).collect();
    glyphs.dedup();
    assert!(
        early.len() >= 10 && glyphs.len() >= 10,
        "the spinner advances before the ACK ({} frames, {} glyph changes):\n{transcript}",
        early.len(),
        glyphs.len()
    );
    let mut secs: Vec<u64> = samples.iter().map(|(_, secs)| *secs).collect();
    secs.dedup();
    assert!(
        secs.windows(2).all(|pair| pair[1] == pair[0] + 1) && secs.starts_with(&[0, 1, 2]),
        "the elapsed counter advances one second at a time from submit: {secs:?}\n{transcript}"
    );
}
