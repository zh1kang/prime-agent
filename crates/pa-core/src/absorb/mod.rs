//! Absorb earlier chats from any local provider into the current one.
//!
//! Layer 0 (`sources`) parses a transcript into a [`DistilledSource`] with no
//! model call. Layer 1 (`distill`) picks the cheapest valid digest method and
//! caches the digest by content (`cache`), next to a turn-indexed
//! `transcript.md` (Layer 2) the model reads on demand. `freshness` compares
//! the source chat's files against the live workspace at absorb time, and
//! `render` turns digests into one byte-stable `[absorbed-context]` packet.

pub mod cache;
pub mod distill;
pub mod freshness;
pub mod render;
pub mod sources;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use cache::{cache_key, Fingerprint};
pub use distill::{Summarizer, ABSORB_PROMPT};
pub use freshness::Freshness;
pub use sources::{CommandRecord, DistilledSource, DistilledTurn, SourceProvider};

pub const SCHEMA_VERSION: u32 = 1;
pub const PROMPT_VERSION: u32 = 1;
/// The default token budget of one combined absorbed row.
pub const DEFAULT_BUDGET_TOKENS: u64 = 6000;

/// One chat to absorb: the provider and its transcript path or thread id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    pub provider: String,
    pub location: String,
}

/// What to absorb and where it lands.
pub struct AbsorbRequest<'a> {
    pub agent_dir: &'a Path,
    /// The live workspace the freshness check compares against.
    pub cwd: &'a Path,
    pub sources: Vec<SourceRef>,
    pub focus: Option<String>,
    pub budget_tokens: u64,
    pub now_ms: u64,
}

/// The cached, source-keyed digest (`digest.json`). It never holds anything
/// that depends on when it is absorbed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Digest {
    pub schema: u32,
    pub prompt_version: u32,
    pub key: String,
    pub method: String,
    pub model: Option<String>,
    pub source: DigestSource,
    pub files: DigestFiles,
    pub commands: Vec<CommandRecord>,
    pub commits: Vec<String>,
    pub summary_md: String,
    pub card_md: String,
    pub transcript: String,
    pub tokens: DigestTokens,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DigestSource {
    pub provider: String,
    pub id: String,
    pub path: String,
    pub title: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub head_commit: Option<String>,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub turns: u32,
    pub raw_bytes: u64,
    pub raw_text_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestFiles {
    pub modified: Vec<String>,
    pub read: Vec<String>,
    /// False when the provider records no tool calls (T3 and pa threads).
    #[serde(default = "known_default")]
    pub known: bool,
}

fn known_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DigestTokens {
    pub packet: u64,
    pub layer0: u64,
    pub raw_text: u64,
}

/// One absorbed source in the outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceOutcome {
    pub key: String,
    pub provider: String,
    pub id: String,
    pub path: String,
    pub title: String,
    pub method: String,
    pub cached: bool,
    pub transcript: String,
    pub tokens: u64,
    pub raw_text_tokens: u64,
}

/// The rendered packet plus what the row's `details` record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbsorbOutcome {
    pub content: String,
    pub tokens: u64,
    /// The single method, or `mixed` across sources.
    pub method: String,
    pub sources: Vec<SourceOutcome>,
    pub changed_files: Vec<String>,
    pub compact: bool,
    /// Sources dropped because an earlier entry had the same key.
    pub duplicates: Vec<String>,
}

/// Absorb every source: Layer 0–2 per source (a cache hit skips parsing and
/// every model call), the freshness check, and one combined packet.
/// `summarizer` is `None` offline or with `--no-llm`.
///
/// # Errors
///
/// Fails when a source cannot be resolved or parsed, or the cache cannot be
/// written.
pub fn absorb(
    request: &AbsorbRequest<'_>,
    mut summarizer: Option<&mut Summarizer<'_>>,
) -> Result<AbsorbOutcome> {
    anyhow::ensure!(
        !request.sources.is_empty(),
        "Name at least one chat to absorb"
    );
    let focus = request
        .focus
        .as_deref()
        .map(str::trim)
        .filter(|focus| !focus.is_empty())
        .map(str::to_owned);
    let mut entries: Vec<(Digest, Freshness)> = Vec::new();
    let mut outcomes = Vec::new();
    let mut duplicates = Vec::new();
    for source in &request.sources {
        let resolved = sources::resolve(source, request.agent_dir)?;
        let fingerprint = Fingerprint::of(&resolved)?;
        let key = cache_key(&resolved, &fingerprint, focus.as_deref());
        if entries.iter().any(|(digest, _)| digest.key == key) {
            duplicates.push(key);
            continue;
        }
        let (digest, cached) = if let Some(digest) = cache::load(request.agent_dir, &key) {
            (digest, true)
        } else {
            let parsed = sources::parse(&resolved).with_context(|| {
                format!(
                    "Could not read {} chat {}",
                    source.provider, source.location
                )
            })?;
            let transcript_md = render::transcript_markdown(&parsed);
            let transcript_path = cache::entry_dir(request.agent_dir, &key).join("transcript.md");
            let digest = distill::digest(
                &parsed,
                &key,
                &transcript_path,
                transcript_md.len() as u64 / 4,
                focus.as_deref(),
                summarizer.as_deref_mut(),
            );
            cache::store(
                request.agent_dir,
                &digest,
                &transcript_md,
                &resolved,
                &fingerprint,
                focus.is_none(),
            )?;
            (digest, false)
        };
        let fresh = freshness::check(&digest, request.cwd, request.now_ms);
        outcomes.push(SourceOutcome {
            key: digest.key.clone(),
            provider: digest.source.provider.clone(),
            id: digest.source.id.clone(),
            path: digest.source.path.clone(),
            title: digest.source.title.clone(),
            method: digest.method.clone(),
            cached,
            transcript: digest.transcript.clone(),
            tokens: digest.tokens.packet,
            raw_text_tokens: digest.source.raw_text_tokens,
        });
        entries.push((digest, fresh));
    }
    let packet = render::render_packet(&entries, request.budget_tokens);
    let mut changed_files: Vec<String> = entries
        .iter()
        .flat_map(|(_, fresh)| fresh.changed_files.iter().cloned())
        .collect();
    changed_files.sort();
    changed_files.dedup();
    let mut methods: Vec<&str> = entries
        .iter()
        .map(|(digest, _)| digest.method.as_str())
        .collect();
    methods.sort_unstable();
    methods.dedup();
    let method = match methods.as_slice() {
        [one] => (*one).to_owned(),
        _ => "mixed".to_owned(),
    };
    Ok(AbsorbOutcome {
        tokens: estimate_tokens(&packet.content),
        content: packet.content,
        method,
        sources: outcomes,
        changed_files,
        compact: packet.compact,
        duplicates,
    })
}

/// Characters / 4, the estimate compaction uses.
#[must_use]
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// `1234` → `1.2k`, `97000` → `97k`.
#[must_use]
pub fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1000..=9999 => format!("{:.1}k", tokens as f64 / 1000.0),
        _ => format!("{}k", (tokens + 500) / 1000),
    }
}

/// Milliseconds since the epoch from an ISO-8601 string or a number of
/// seconds or milliseconds.
#[must_use]
pub fn parse_timestamp(value: &serde_json::Value) -> Option<u64> {
    if let Some(number) = value.as_f64() {
        if number <= 0.0 {
            return None;
        }
        return Some(if number > 1e12 {
            number as u64
        } else {
            (number * 1000.0) as u64
        });
    }
    parse_iso8601(value.as_str()?)
}

fn parse_iso8601(text: &str) -> Option<u64> {
    let text = text.trim();
    let num = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = text.get(19..)?;
    let mut millis = 0i64;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits: String = fraction.chars().take_while(char::is_ascii_digit).collect();
        rest = &fraction[digits.len()..];
        let padded = format!("{digits:0<3}");
        millis = padded[..3].parse().ok()?;
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        zone => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let hours: i64 = zone.get(1..3)?.parse().ok()?;
            let minutes: i64 = zone.get(zone.len() - 2..)?.parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds * 1000 + millis).ok()
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month, day, hour, minute)` in UTC.
#[must_use]
pub fn utc_parts(ms: u64) -> (i64, i64, i64, i64, i64) {
    let seconds = (ms / 1000) as i64;
    let days = seconds.div_euclid(86400);
    let rem = seconds.rem_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, rem / 3600, rem % 3600 / 60)
}

/// `2026-11-22 14:02 UTC`.
#[must_use]
pub fn format_utc(ms: u64) -> String {
    let (y, mo, d, h, mi) = utc_parts(ms);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// The canonical form of a path when it exists, else the path itself.
#[must_use]
pub fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn claude_chat(dir: &Path, name: &str, user: &str) -> PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        for (id, parent, role, content) in [
            ("a", None, "user", serde_json::json!(user)),
            (
                "b",
                Some("a"),
                "assistant",
                serde_json::json!([{"type":"text","text":"done"}]),
            ),
        ] {
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":role,"uuid":id,"parentUuid":parent,"sessionId":name,"cwd":"/nowhere/app","timestamp":"2026-11-22T14:00:00Z","message":{"role":role,"content":content}})
            )
            .unwrap();
        }
        path
    }

    fn request<'a>(agent: &'a Path, cwd: &'a Path, paths: &[&Path]) -> AbsorbRequest<'a> {
        AbsorbRequest {
            agent_dir: agent,
            cwd,
            sources: paths
                .iter()
                .map(|path| SourceRef {
                    provider: "claude".into(),
                    location: path.display().to_string(),
                })
                .collect(),
            focus: None,
            budget_tokens: DEFAULT_BUDGET_TOKENS,
            now_ms: 1_795_400_000_000,
        }
    }

    #[test]
    fn a_second_absorb_hits_the_cache_without_a_summarizer_call() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        let chat = claude_chat(dir.path(), "long.jsonl", &"word ".repeat(4000));
        let mut summarizer =
            Summarizer::new("faux/m", 200_000, |_: &str| Ok("## Goal\nG".to_owned()));
        let first = absorb(
            &request(&agent, dir.path(), &[&chat]),
            Some(&mut summarizer),
        )
        .unwrap();
        assert_eq!((first.method.as_str(), summarizer.calls), ("distill", 1));
        assert!(!first.sources[0].cached);
        let transcript = std::fs::read_to_string(&first.sources[0].transcript).unwrap();
        assert!(transcript.contains("## Turn 1 · assistant (final)\ndone"));
        let second = absorb(
            &request(&agent, dir.path(), &[&chat]),
            Some(&mut summarizer),
        )
        .unwrap();
        assert_eq!(summarizer.calls, 1, "a cache hit makes no summarizer call");
        assert!(second.sources[0].cached);
        assert_eq!(first.content, second.content);
        // The history picker's pointer names the cached key.
        let pointer = cache::pointer_path(&agent, SourceProvider::Claude, &canonical(&chat));
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(pointer).unwrap()).unwrap();
        assert_eq!(record["key"], first.sources[0].key.as_str());
        // A deleted source worktree is a note, never a failure.
        assert!(first.content.contains("/nowhere/app (no longer present)"));
    }

    #[test]
    fn several_chats_make_one_row_and_duplicates_drop() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        let one = claude_chat(dir.path(), "one.jsonl", "first task");
        let two = claude_chat(dir.path(), "two.jsonl", "second task");
        let outcome = absorb(&request(&agent, dir.path(), &[&one, &two, &one]), None).unwrap();
        assert_eq!(outcome.sources.len(), 2);
        assert_eq!(outcome.duplicates.len(), 1);
        assert_eq!(outcome.method, "verbatim");
        assert_eq!(outcome.content.matches("<absorbed source=").count(), 2);
        assert!(outcome.content.starts_with(render::PACKET_HEADER));
    }

    #[test]
    fn timestamps_round_trip_in_utc() {
        let ms = parse_timestamp(&serde_json::json!("2026-11-22T14:02:00.5Z")).unwrap();
        assert_eq!(format_utc(ms), "2026-11-22 14:02 UTC");
        assert_eq!(
            parse_timestamp(&serde_json::json!("2026-11-22T09:02:00-05:00")),
            Some(ms - 500)
        );
        assert_eq!(format_tokens(97_000), "97k");
        assert_eq!(format_tokens(1412), "1.4k");
    }
}
