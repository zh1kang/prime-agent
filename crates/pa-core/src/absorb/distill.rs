//! Layer 1: the cheapest valid digest. `verbatim` and `reuse` need no model
//! call, `distill` makes one summarizer call, `fold` summarizes in chunks when
//! the chat exceeds the summarizer's window, and `degraded` is a deterministic
//! card for when no model is available (or a call fails).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use super::sources::{clip, DistilledSource, DistilledTurn};
use super::{
    estimate_tokens, Digest, DigestFiles, DigestSource, DigestTokens, PROMPT_VERSION,
    SCHEMA_VERSION,
};

/// Chats at most this long are absorbed verbatim.
pub const VERBATIM_MAX_TOKENS: u64 = 3000;
/// A native summary is reused when the turns after it stay this short.
pub const REUSE_TAIL_MAX_TOKENS: u64 = 2500;
/// The summarizer's output cap.
pub const MAX_OUTPUT_TOKENS: u64 = 2048;
/// Fold chunks stay at most this large.
const FOLD_CHUNK_TOKENS: u64 = 60_000;
/// Room the prompt and the reply take from the summarizer's window.
const PROMPT_OVERHEAD_TOKENS: u64 = 4096;

pub const ABSORB_PROMPT: &str = "Summarize the earlier coding conversation below so a new chat can continue the work without rereading it. Use exactly these sections:

## Goal
What the user is trying to accomplish.

## Decisions & Constraints
Each as **decision** — rationale. Quote user preferences verbatim.

## State at End
### Done
### In Progress
### Blocked

## Exact Code State
File → symbol → what changed. Preserve exact paths, function names, and error messages.

## Open Bugs / TODOs / Next Steps

## Dead Ends — do not retry
Approach → why it failed, one line each.

Be dense and specific. Omit empty subsections. Do not continue the conversation.";

/// A one-off summarizer: the model label, its context window, and the call
/// (prompt in, summary out, capped at [`MAX_OUTPUT_TOKENS`]).
pub struct Summarizer<'a> {
    pub model: String,
    pub context_window: u64,
    pub call: Box<dyn FnMut(&str) -> Result<String> + Send + 'a>,
    /// Calls made so far.
    pub calls: u32,
}

impl<'a> Summarizer<'a> {
    pub fn new(
        model: impl Into<String>,
        context_window: u64,
        call: impl FnMut(&str) -> Result<String> + Send + 'a,
    ) -> Self {
        Self {
            model: model.into(),
            context_window,
            call: Box::new(call),
            calls: 0,
        }
    }

    fn summarize(&mut self, prompt: &str) -> Result<String> {
        self.calls += 1;
        (self.call)(prompt)
    }
}

/// One summarizer call on a routed model: SSE, minimum reasoning, at most
/// [`MAX_OUTPUT_TOKENS`] out, never on the session's prompt-cache prefix.
///
/// # Errors
///
/// Fails when the provider call fails or returns no text.
pub async fn summary_call(
    target: &crate::session_engine::auxiliary_model::ResolvedAuxiliaryModel,
    session_id: &str,
    prompt: &str,
) -> Result<String> {
    let request = vec![pa_types::session::AgentMessage::User(
        pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text(prompt.to_owned()),
            timestamp: 0,
            rest: serde_json::Map::default(),
        },
    )];
    let slice = crate::session_engine::compaction_exec::complete_summary_call(
        &target.model,
        session_id,
        target.api_key.clone(),
        target.headers.clone(),
        MAX_OUTPUT_TOKENS,
        request,
        None,
        "Absorb summary failed",
    )
    .await?;
    Ok(slice.summary)
}

/// Build the digest for a parsed source.
#[must_use]
pub fn digest(
    source: &DistilledSource,
    key: &str,
    transcript_path: &Path,
    layer0_tokens: u64,
    focus: Option<&str>,
    summarizer: Option<&mut Summarizer<'_>>,
) -> Digest {
    let mut warnings = source.warnings.clone();
    let mut model = None;
    let (method, summary_md) = if source.raw_text_tokens <= VERBATIM_MAX_TOKENS && focus.is_none() {
        ("verbatim", verbatim(&source.turns))
    } else if let Some(summary) = source
        .existing_summary
        .as_deref()
        .filter(|_| source.tail_tokens() <= REUSE_TAIL_MAX_TOKENS)
    {
        let tail = &source.turns[source.summary_turn_index..];
        let mut text = summary.trim().to_owned();
        if !tail.is_empty() {
            let _ = write!(text, "\n\n## Since that summary\n{}", verbatim(tail));
        }
        ("reuse", text)
    } else if let Some(summarizer) = summarizer {
        match summarize(source, focus, summarizer) {
            Ok((method, text)) => {
                model = Some(summarizer.model.clone());
                (method, text)
            }
            Err(error) => {
                warnings.push(format!(
                    "Summarizer failed ({error:#}); used the offline card"
                ));
                ("degraded", degraded(source))
            }
        }
    } else {
        ("degraded", degraded(source))
    };
    let card_md = card(&summary_md);
    let modified: Vec<String> = source.files_modified.iter().cloned().collect();
    let read: Vec<String> = source.files_read.iter().cloned().collect();
    let file_chars: usize = modified
        .iter()
        .chain(&read)
        .map(|path| path.len() + 1)
        .sum();
    let packet = estimate_tokens(&summary_md) + (file_chars as u64).div_ceil(4) + 80;
    Digest {
        schema: SCHEMA_VERSION,
        prompt_version: PROMPT_VERSION,
        key: key.to_owned(),
        method: method.to_owned(),
        model,
        source: DigestSource {
            provider: source.provider().as_str().to_owned(),
            id: source.id.clone(),
            path: source.path.display().to_string(),
            title: source.title.clone(),
            cwd: source.cwd.as_ref().map(|cwd| cwd.display().to_string()),
            git_branch: source.git_branch.clone(),
            head_commit: source.head_commit.clone(),
            started_ms: source.started_ms,
            ended_ms: source.ended_ms,
            turns: source.turns.len() as u32,
            raw_bytes: source.raw_bytes,
            raw_text_tokens: source.raw_text_tokens,
        },
        files: DigestFiles {
            modified,
            read,
            known: source.files_known,
        },
        commands: source.commands.clone(),
        commits: source.commits.clone(),
        summary_md,
        card_md,
        transcript: transcript_path.display().to_string(),
        tokens: DigestTokens {
            packet,
            layer0: layer0_tokens,
            raw_text: source.raw_text_tokens,
        },
        warnings,
    }
}

/// One call when the conversation fits the window, else a chunked fold that
/// carries the running summary forward.
fn summarize(
    source: &DistilledSource,
    focus: Option<&str>,
    summarizer: &mut Summarizer<'_>,
) -> Result<(&'static str, String)> {
    let window = summarizer.context_window.max(16_000);
    let budget = (window * 3 / 4)
        .saturating_sub(PROMPT_OVERHEAD_TOKENS)
        .max(8_000);
    // A native summary stands in for the turns before it.
    let (mut previous, turns) = match &source.existing_summary {
        Some(summary) => (
            Some(summary.clone()),
            &source.turns[source.summary_turn_index..],
        ),
        None => (None, source.turns.as_slice()),
    };
    let text = conversation_text(turns);
    if estimate_tokens(&text) <= budget {
        let reply = summarizer.summarize(&prompt(&text, focus, previous.as_deref()))?;
        return Ok(("distill", reply.trim().to_owned()));
    }
    let chunk_budget = budget.min(FOLD_CHUNK_TOKENS);
    let mut chunk = String::new();
    for turn in turns {
        let piece = conversation_text(std::slice::from_ref(turn));
        if !chunk.is_empty() && estimate_tokens(&chunk) + estimate_tokens(&piece) > chunk_budget {
            previous = Some(summarizer.summarize(&prompt(&chunk, focus, previous.as_deref()))?);
            chunk.clear();
        }
        // A single turn larger than a chunk keeps its head and tail.
        chunk.push_str(&super::sources::clip_middle(
            &piece,
            (chunk_budget * 4) as usize,
        ));
    }
    if !chunk.is_empty() {
        previous = Some(summarizer.summarize(&prompt(&chunk, focus, previous.as_deref()))?);
    }
    Ok(("fold", previous.unwrap_or_default().trim().to_owned()))
}

fn prompt(conversation: &str, focus: Option<&str>, previous: Option<&str>) -> String {
    let mut out = String::from(ABSORB_PROMPT);
    if let Some(focus) = focus {
        let _ = write!(
            out,
            "\n\nFocus: prioritize everything related to \"{focus}\"; compress unrelated work to one line."
        );
    }
    if let Some(previous) = previous {
        let _ = write!(
            out,
            "\n\nThe earlier part of the conversation is already summarized below. Update that summary with the new turns, keeping its sections.\n<previous-summary>\n{previous}\n</previous-summary>"
        );
    }
    let _ = write!(out, "\n\n<conversation>\n{conversation}\n</conversation>");
    out
}

/// The summarizer's input: user text, final assistant text, tool lines, and
/// clipped errors per turn.
#[must_use]
pub fn conversation_text(turns: &[DistilledTurn]) -> String {
    let mut out = String::new();
    for turn in turns {
        let _ = writeln!(out, "[Turn {}]", turn.index);
        if !turn.user.is_empty() {
            let _ = writeln!(out, "[User]: {}", turn.user);
        }
        for line in turn.tools.iter().chain(&turn.errors) {
            let _ = writeln!(out, "{line}");
        }
        if !turn.assistant.is_empty() {
            let _ = writeln!(out, "[Assistant]: {}", turn.assistant);
        }
    }
    out
}

fn verbatim(turns: &[DistilledTurn]) -> String {
    let mut out = String::new();
    for turn in turns {
        let _ = writeln!(out, "### Turn {}", turn.index);
        if !turn.user.is_empty() {
            let _ = writeln!(out, "User: {}", turn.user.trim());
        }
        if !turn.assistant.is_empty() {
            let _ = writeln!(out, "Assistant: {}", turn.assistant.trim());
        }
    }
    out.trim_end().to_owned()
}

/// The offline card: the first ask, the last exchange, and a pointer to the
/// transcript (files and commands render beside it).
fn degraded(source: &DistilledSource) -> String {
    let first = source
        .turns
        .iter()
        .find(|turn| !turn.user.is_empty())
        .map(|turn| clip(turn.user.trim(), 1500))
        .unwrap_or_default();
    let last_user = source
        .turns
        .iter()
        .rev()
        .find(|turn| !turn.user.is_empty())
        .map(|turn| clip(turn.user.trim(), 1000))
        .unwrap_or_default();
    let last_assistant = source
        .turns
        .iter()
        .rev()
        .find(|turn| !turn.assistant.is_empty())
        .map(|turn| clip(turn.assistant.trim(), 1000))
        .unwrap_or_default();
    let mut out = format!("## Goal\n{first}\n\n## State at End\n");
    if let Some(summary) = &source.existing_summary {
        let _ = writeln!(
            out,
            "Earlier summary from the source chat:\n{}\n",
            clip(summary.trim(), 2000)
        );
    }
    if !last_user.is_empty() && last_user != first {
        let _ = writeln!(out, "Last user message: {last_user}");
    }
    if !last_assistant.is_empty() {
        let _ = writeln!(out, "Last assistant message: {last_assistant}");
    }
    out.push_str(
        "\n## Note\nDistilled offline without a summarizer model; read the transcript for detail.",
    );
    out
}

/// The compact form a budget-limited row uses: Goal, State at End, and Dead
/// Ends, or the clipped summary when it has none of them.
#[must_use]
pub fn card(summary: &str) -> String {
    let mut kept = Vec::new();
    let mut keep = false;
    for line in summary.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            keep = ["Goal", "State at End", "Dead Ends"]
                .iter()
                .any(|name| heading.trim_start().starts_with(name));
        }
        if keep {
            kept.push(line);
        }
    }
    let text = if kept.is_empty() {
        summary.to_owned()
    } else {
        kept.join("\n")
    };
    clip(text.trim(), 1600)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(index: u32, user: &str, assistant: &str) -> DistilledTurn {
        DistilledTurn {
            index,
            user: user.into(),
            assistant: assistant.into(),
            ..DistilledTurn::default()
        }
    }

    fn source(turns: Vec<DistilledTurn>, raw_text_tokens: u64) -> DistilledSource {
        DistilledSource {
            id: "s".into(),
            turns,
            raw_text_tokens,
            files_known: true,
            ..DistilledSource::default()
        }
    }

    #[test]
    fn picks_the_cheapest_valid_method() {
        let path = Path::new("/t.md");
        let short = source(vec![turn(1, "hi", "hello")], 10);
        assert_eq!(digest(&short, "k", path, 5, None, None).method, "verbatim");

        let long_text = "x".repeat(20_000);
        let mut native = source(vec![turn(1, &long_text, "a"), turn(2, "tail", "ok")], 5_000);
        native.existing_summary = Some("## Goal\nship".into());
        native.summary_turn_index = 1;
        let reused = digest(&native, "k", path, 5, None, None);
        assert_eq!(reused.method, "reuse");
        assert!(
            reused.summary_md.starts_with("## Goal\nship")
                && reused.summary_md.contains("User: tail")
        );

        let long = source(vec![turn(1, &long_text, "a")], 5_000);
        assert_eq!(digest(&long, "k", path, 5, None, None).method, "degraded");
        let mut summarizer = Summarizer::new("faux/model", 200_000, |prompt: &str| {
            assert!(prompt.contains("## Dead Ends — do not retry"));
            Ok("## Goal\nG\n## State at End\nS\n## Exact Code State\nE".to_owned())
        });
        let distilled = digest(&long, "k", path, 5, None, Some(&mut summarizer));
        assert_eq!(
            (distilled.method.as_str(), summarizer.calls),
            ("distill", 1)
        );
        assert_eq!(distilled.model.as_deref(), Some("faux/model"));
        assert_eq!(distilled.card_md, "## Goal\nG\n## State at End\nS");

        // A focus filters even a short chat through the summarizer.
        let mut focused = Summarizer::new("faux/model", 200_000, |prompt: &str| {
            assert!(prompt.contains("Focus: prioritize everything related to \"auth\""));
            Ok("## Goal\nauth".to_owned())
        });
        assert_eq!(
            digest(&short, "k", path, 5, Some("auth"), Some(&mut focused)).method,
            "distill"
        );

        let mut failing =
            Summarizer::new("faux/model", 200_000, |_: &str| anyhow::bail!("offline"));
        let fallback = digest(&long, "k", path, 5, None, Some(&mut failing));
        assert_eq!(fallback.method, "degraded");
        assert!(fallback.warnings[0].contains("offline"));
    }

    #[test]
    fn folds_a_chat_larger_than_the_window() {
        let turns: Vec<DistilledTurn> = (1..=6)
            .map(|i| turn(i, &"y".repeat(40_000), "done"))
            .collect();
        let big = source(turns, 60_000);
        let mut seen_previous = 0;
        let mut summarizer = Summarizer::new("faux/small", 16_000, |prompt: &str| {
            if prompt.contains("<previous-summary>") {
                seen_previous += 1;
            }
            Ok("## Goal\nfolded".to_owned())
        });
        let digest = digest(
            &big,
            "k",
            Path::new("/t.md"),
            5,
            None,
            Some(&mut summarizer),
        );
        assert_eq!(digest.method, "fold");
        assert!(summarizer.calls > 1);
        drop(summarizer);
        assert!(seen_previous >= 1);
    }
}
