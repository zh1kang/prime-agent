//! The detail-level view of a thinking block: one pure transform that both the
//! paint (`render_assistant_with_style`) and the count (`assistant_row_count`)
//! consume, so the rendered and counted rows stay identical.
//!
//! CLI providers interleave one-line tool markers (`▸ tool …`, `✗ …`) with long
//! reasoning prose inside a thinking block. `overview` keeps only the markers,
//! `details` keeps the markers and folds each prose run to one summary line (or
//! a short rolling tail while the block streams), and `all` keeps the text verbatim.
use super::Detail;
use std::borrow::Cow;

/// Prose runs at or under this many lines show verbatim while they stream.
const STREAMING_VERBATIM_LINES: usize = 3;
/// The rolling tail a long streaming prose run shows below its header.
const STREAMING_TAIL_LINES: usize = 2;
/// A settled single prose line at or under this width shows verbatim.
const SHORT_LINE_CHARS: usize = 120;
/// The summary snippet's maximum length, in chars.
const SNIPPET_CHARS: usize = 96;

/// Whether a thinking line is a tool progress marker.
fn is_tool_marker(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with('\u{25b8}') || line.starts_with('\u{2717}')
}

/// Whether `format_thinking_for_detail` yields any text, without building it.
pub(crate) fn thinking_is_visible(text: &str, detail: Detail) -> bool {
    match detail {
        Detail::Overview => text.lines().any(is_tool_marker),
        Detail::Details | Detail::All => !text.trim().is_empty(),
    }
}

/// The thinking text shown at `detail`, or `None` when nothing shows.
/// `streaming_last` marks the final block of a still-streaming message.
pub(crate) fn format_thinking_for_detail(
    text: &str,
    detail: Detail,
    streaming_last: bool,
) -> Option<Cow<'_, str>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    match detail {
        Detail::All => Some(Cow::Borrowed(text)),
        Detail::Overview => {
            let markers: Vec<&str> = text
                .lines()
                .filter(|line| is_tool_marker(line))
                .map(str::trim)
                .collect();
            (!markers.is_empty()).then(|| Cow::Owned(markers.join("\n")))
        }
        Detail::Details => Some(Cow::Owned(details_view(text, streaming_last))),
    }
}

/// The `details` view: tool markers verbatim, each prose run condensed.
fn details_view(text: &str, streaming_last: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut run: Vec<&str> = Vec::new();
    for line in text.lines() {
        if is_tool_marker(line) {
            condense_run(&run, false, &mut out);
            run.clear();
            out.push(line.trim().to_string());
        } else if !line.trim().is_empty() {
            run.push(line.trim_end());
        }
    }
    condense_run(&run, streaming_last, &mut out);
    out.join("\n")
}

/// Append one prose run's condensed lines. `streaming` marks the run still streaming.
fn condense_run(run: &[&str], streaming: bool, out: &mut Vec<String>) {
    match run {
        [] => {}
        _ if streaming && run.len() <= STREAMING_VERBATIM_LINES => {
            out.extend(run.iter().map(|line| (*line).to_string()));
        }
        _ if streaming => {
            let earlier = run.len() - STREAMING_TAIL_LINES;
            out.push(format!(
                "\u{2234} Thinking\u{2026} (+{earlier} earlier {} \u{b7} Ctrl+O for all)",
                plural(earlier)
            ));
            out.extend(run[earlier..].iter().map(|line| (*line).to_string()));
        }
        [line] if line.trim().chars().count() <= SHORT_LINE_CHARS => {
            out.push((*line).to_string());
        }
        _ => {
            let snippet = summary_snippet(run);
            let extra = run.len() - 1;
            if extra == 0 {
                out.push(format!("\u{2234} {snippet}"));
            } else {
                out.push(format!("\u{2234} {snippet} (+{extra} {})", plural(extra)));
            }
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        "line"
    } else {
        "lines"
    }
}

/// The run's first bold heading (`**…**`), else its first line, as plain text
/// truncated to `SNIPPET_CHARS`.
fn summary_snippet(run: &[&str]) -> String {
    let heading = run.iter().find_map(|line| {
        let start = line.find("**")?;
        let rest = &line[start + 2..];
        let end = rest.find("**")?;
        let inner = rest[..end].trim();
        (!inner.is_empty()).then_some(inner)
    });
    let source = heading.unwrap_or(run[0]);
    truncate_chars(&strip_markdown(source), SNIPPET_CHARS)
}

/// Drop block prefixes (`#`, `>`, list bullets) and inline emphasis/code marks.
fn strip_markdown(line: &str) -> String {
    let mut rest = line.trim();
    loop {
        let unquoted = rest.trim_start_matches(['#', '>']).trim_start();
        let next = unquoted
            .strip_prefix("- ")
            .or_else(|| unquoted.strip_prefix("* "))
            .unwrap_or(unquoted);
        if next == rest {
            break;
        }
        rest = next;
    }
    let stripped: String = rest.chars().filter(|c| !matches!(c, '*' | '`')).collect();
    let stripped = stripped.replace("__", "");
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut at a word boundary when one is close, ending with `…`.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    let cut = match cut.rfind(' ') {
        Some(space) if cut[..space].chars().count() >= max * 2 / 3 => &cut[..space],
        _ => cut.as_str(),
    };
    format!(
        "{}\u{2026}",
        cut.trim_end_matches([' ', ',', ';', ':', '.', '-'])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(text: &str, detail: Detail, streaming_last: bool) -> Option<String> {
        format_thinking_for_detail(text, detail, streaming_last).map(Cow::into_owned)
    }

    #[test]
    fn tool_markers_are_classified_by_their_leading_glyph() {
        assert!(is_tool_marker("\u{25b8} write_file a.rs"));
        assert!(is_tool_marker("   \u{2717} tool failed"));
        assert!(!is_tool_marker("thinking about \u{25b8} later"));
        assert!(!is_tool_marker(""));
    }

    #[test]
    fn overview_keeps_only_tool_markers() {
        let text = "Plan the edit.\n\nMore prose.\n\u{25b8} write_file a.rs\n\nprose\n\u{2717} tool failed";
        assert_eq!(
            fmt(text, Detail::Overview, false).as_deref(),
            Some("\u{25b8} write_file a.rs\n\u{2717} tool failed")
        );
        assert_eq!(fmt("only prose\n\nhere", Detail::Overview, false), None);
        assert_eq!(fmt("   ", Detail::Overview, false), None);
    }

    #[test]
    fn all_is_verbatim() {
        let text = "  **Plan**\n\nline one\nline two\n\u{25b8} run_command ls\n";
        assert_eq!(fmt(text, Detail::All, false).as_deref(), Some(text.trim()));
        assert_eq!(fmt("\n\n", Detail::All, false), None);
    }

    #[test]
    fn details_collapses_settled_prose_runs_and_keeps_markers() {
        let text = "**Inspecting the parser**\n\nI need to read the file first.\n\nThen edit it.\n\u{25b8} read_file src/lib.rs\nShort note.\n\u{25b8} write_file src/lib.rs\nFirst of two.\nSecond of two.";
        assert_eq!(
            fmt(text, Detail::Details, false).as_deref(),
            Some(
                "\u{2234} Inspecting the parser (+2 lines)\n\
                 \u{25b8} read_file src/lib.rs\n\
                 Short note.\n\
                 \u{25b8} write_file src/lib.rs\n\
                 \u{2234} First of two. (+1 line)"
            )
        );
    }

    #[test]
    fn details_summarizes_one_long_line() {
        let long = format!("`alpha` {}", "word ".repeat(40));
        let out = fmt(&long, Detail::Details, false).unwrap();
        assert!(out.starts_with("\u{2234} alpha word"), "{out}");
        assert!(out.ends_with('\u{2026}'), "{out}");
        assert!(!out.contains("(+"), "{out}");
        assert!(out.chars().count() <= SNIPPET_CHARS + 2, "{out}");
    }

    #[test]
    fn details_streaming_shows_a_rolling_tail() {
        let paragraphs: Vec<String> = (1..=20).map(|n| format!("paragraph {n}")).collect();
        let text = paragraphs.join("\n\n");
        assert_eq!(
            fmt(&text, Detail::Details, true).as_deref(),
            Some(
                "\u{2234} Thinking\u{2026} (+18 earlier lines \u{b7} Ctrl+O for all)\n\
                 paragraph 19\nparagraph 20"
            )
        );
        // A short live run shows verbatim.
        assert_eq!(
            fmt("one\n\ntwo\nthree", Detail::Details, true).as_deref(),
            Some("one\ntwo\nthree")
        );
        // Only the final run is live; a run before a tool marker is settled.
        let mixed = "a\nb\nc\nd\n\u{25b8} run_command ls\ne\nf";
        assert_eq!(
            fmt(mixed, Detail::Details, true).as_deref(),
            Some("\u{2234} a (+3 lines)\n\u{25b8} run_command ls\ne\nf")
        );
    }

    #[test]
    fn visibility_predicate_matches_the_transform() {
        let samples = [
            "",
            "  \n ",
            "prose",
            "prose\n\nmore",
            "\u{25b8} write_file a.rs",
            "prose\n  \u{2717} failed",
        ];
        for text in samples {
            for detail in [Detail::Overview, Detail::Details, Detail::All] {
                for streaming in [false, true] {
                    assert_eq!(
                        thinking_is_visible(text, detail),
                        format_thinking_for_detail(text, detail, streaming).is_some(),
                        "{text:?} {detail:?} {streaming}"
                    );
                }
            }
        }
    }

    #[test]
    fn snippet_strips_markdown_prefixes() {
        assert_eq!(strip_markdown("## > - `code` and *em*"), "code and em");
        assert_eq!(truncate_chars("short", 10), "short");
        let cut = truncate_chars(&"abcdefghij ".repeat(20), 30);
        assert!(
            cut.ends_with('\u{2026}') && cut.chars().count() <= 30,
            "{cut}"
        );
    }
}
