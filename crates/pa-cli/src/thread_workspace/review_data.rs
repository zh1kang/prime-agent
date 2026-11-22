//! Worktree review data for pa's native review panel, backed by Diffy's unified-patch model.
//! `pa thread [ID] review json` reads one JSON request on stdin and answers with JSON.
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use diffy_diff::{
    parse_unified_checked, DiffDocument, DiffLimits, Expansion, Mode, Projection, RowKind, Side,
};
use pa_core::platform::command_output::output_with_timeout;
use serde_json::{json, Value};

use super::git;

const LIMIT: usize = 4 * 1024 * 1024;

struct Entry {
    path: PathBuf,
    index: u8,
    worktree: u8,
}

impl Entry {
    fn visible(&self, staged: bool) -> bool {
        if staged {
            !matches!(self.index, b' ' | b'?')
        } else {
            self.worktree != b' '
        }
    }
}

fn output(cmd: &mut Command, accept_diff: bool) -> Result<Vec<u8>> {
    let out = output_with_timeout(cmd, Duration::from_secs(30), LIMIT)?;
    ensure!(!out.timed_out, "Git operation timed out");
    ensure!(
        out.succeeded() || (accept_diff && out.status == Some(1)),
        "Git: {}",
        clean(&out.stderr_text())
    );
    ensure!(!out.truncated, "Output exceeds the 4 MiB review limit");
    Ok(out.stdout)
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(unix)]
fn path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes.to_vec()).into()
}

#[cfg(not(unix))]
fn path(bytes: &[u8]) -> PathBuf {
    OsString::from(String::from_utf8_lossy(bytes).into_owned()).into()
}

fn entries(root: &Path, staged: bool) -> Result<Vec<Entry>> {
    let bytes = output(
        git(root).args([
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=all",
        ]),
        false,
    )?;
    let mut result = vec![];
    for row in bytes.split(|c| *c == 0).filter(|r| !r.is_empty()) {
        ensure!(
            row.len() > 3 && row[2] == b' ',
            "Unexpected Git status record"
        );
        let entry = Entry {
            index: row[0],
            worktree: row[1],
            path: path(&row[3..]),
        };
        if entry.visible(staged) {
            result.push(entry);
        }
    }
    Ok(result)
}

fn patch(root: &Path, entry: &Entry, staged: bool) -> Result<DiffDocument> {
    let mut cmd = git(root);
    cmd.args([
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--submodule=short",
    ]);
    let untracked = entry.index == b'?';
    if untracked {
        // Never ask Git to walk an untracked nested repository or open a special device.
        let kind = std::fs::symlink_metadata(root.join(&entry.path))?.file_type();
        ensure!(
            kind.is_file() || kind.is_symlink(),
            "Untracked directory or special file"
        );
        cmd.args([
            "--no-index",
            "--",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        ]);
    } else {
        if staged {
            cmd.arg("--cached");
        }
        cmd.arg("--");
    }
    let data = output(cmd.arg(&entry.path), untracked)?;
    let text = String::from_utf8_lossy(&data);
    let limits = DiffLimits {
        input_bytes: LIMIT as u64,
        input_lines: 100_000,
        ..DiffLimits::default()
    };
    Ok(parse_unified_checked(&text, &limits)?)
}

/// One side of a diff row: line number, sign, and text.
fn cell(doc: &DiffDocument, rows: &Projection, row: usize, side: Side) -> Value {
    let file = rows.file[row];
    let Some(index) = rows.line(row as u32, side) else {
        return Value::Null;
    };
    let sign = match (side, rows.kind[row]) {
        (Side::Old, RowKind::Removed | RowKind::Modified) => "-",
        (Side::New, RowKind::Added | RowKind::Modified) => "+",
        _ => " ",
    };
    let hunk = rows.hunk[row] as usize;
    let first_block = doc.hunks().blocks[hunk].start as usize;
    let blocks = doc.blocks();
    let number = match side {
        Side::Old => blocks.old_line[first_block] + (index - blocks.old_store[first_block]),
        Side::New => blocks.new_line[first_block] + (index - blocks.new_store[first_block]),
    };
    let text = doc.text(file, side).display_line(index).unwrap_or("");
    json!({"n": number, "sign": sign, "text": clean(text)})
}

/// The projected rows of one file's diff in `mode`, as the panel draws them.
fn diff_rows(doc: &DiffDocument, mode: Mode) -> Vec<Value> {
    let rows = Projection::new(doc, mode, &Expansion::new(doc));
    (0..rows.kind.len())
        .map(|i| match rows.kind[i] {
            RowKind::FileHeader => {
                let facts = doc.facts(rows.file[i]);
                let text = if facts.binary {
                    "Binary file".to_owned()
                } else if let Some((old, new)) = facts.mode_change.as_ref() {
                    format!("Mode {old} → {new}")
                } else if !facts.has_hunks {
                    "Metadata change".to_owned()
                } else {
                    String::new()
                };
                json!({"kind": "file", "text": text})
            }
            RowKind::HunkHeader => {
                let h = rows.hunk[i] as usize;
                let hs = doc.hunks();
                json!({"kind": "hunk", "text": format!(
                    "@@ -{},{} +{},{} @@ {}",
                    hs.old_start[h], hs.old_len[h], hs.new_start[h], hs.new_len[h], clean(&hs.section[h])
                )})
            }
            RowKind::Gap => json!({"kind": "gap"}),
            kind => {
                let name = match kind {
                    RowKind::Added => "added",
                    RowKind::Removed => "removed",
                    RowKind::Modified => "modified",
                    _ => "context",
                };
                json!({"kind": name, "old": cell(doc, &rows, i, Side::Old), "new": cell(doc, &rows, i, Side::New)})
            }
        })
        .collect()
}

fn find<'a>(files: &'a [Entry], wanted: &str) -> Result<&'a Entry> {
    files
        .iter()
        .find(|entry| entry.path == Path::new(wanted))
        .context("This file has no changes in this view. Refresh the list.")
}

/// Answer one review request: `files`, `diff`, `stage`, or `unstage`.
pub(super) fn answer(root: &Path, request: &Value) -> Result<Value> {
    let staged = request["staged"].as_bool();
    match request["op"].as_str() {
        Some("files") => {
            // With nothing unstaged, open on the staged view.
            let mut staged = staged.unwrap_or(false);
            let mut files = entries(root, staged)?;
            if files.is_empty() && request["staged"].is_null() {
                let other = entries(root, true)?;
                if !other.is_empty() {
                    staged = true;
                    files = other;
                }
            }
            let files: Vec<Value> = files
                .iter()
                .map(|e| {
                    let code = if staged { e.index } else { e.worktree };
                    json!({"path": e.path.to_string_lossy(), "status": char::from(code).to_string()})
                })
                .collect();
            Ok(json!({"root": root, "staged": staged, "files": files}))
        }
        Some("diff") => {
            let staged = staged.unwrap_or(false);
            let files = entries(root, staged)?;
            let entry = find(&files, request["path"].as_str().unwrap_or_default())?;
            let mode = if request["mode"] == "unified" {
                Mode::Unified
            } else {
                Mode::Split
            };
            Ok(json!({"rows": diff_rows(&patch(root, entry, staged)?, mode)}))
        }
        Some(op @ ("stage" | "unstage")) => {
            let files = entries(root, op == "unstage")?;
            let entry = find(&files, request["path"].as_str().unwrap_or_default())?;
            let mut cmd = git(root);
            if op == "unstage" {
                cmd.args(["reset", "--quiet"]);
            } else {
                cmd.arg("add");
            }
            output(cmd.arg("--").arg(&entry.path), false)?;
            Ok(json!({"ok": true}))
        }
        _ => bail!("Review requests: files, diff, stage, unstage"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_diff_and_stage_answer_as_json() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path();
        let git_ok = |args: &[&str]| assert!(git(repo).args(args).status().unwrap().success());
        git_ok(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n")?;
        git_ok(&["add", "a.txt"]);
        git_ok(&[
            "-c",
            "user.name=F",
            "-c",
            "user.email=f@example.invalid",
            "commit",
            "-qm",
            "base",
        ]);
        std::fs::write(repo.join("a.txt"), "one\nTWO\n")?;
        let files = answer(repo, &json!({"op": "files"}))?;
        assert_eq!(files["staged"], false);
        assert_eq!(files["files"], json!([{"path": "a.txt", "status": "M"}]));
        let diff = answer(
            repo,
            &json!({"op": "diff", "path": "a.txt", "mode": "unified"}),
        )?;
        let rows = diff["rows"].as_array().unwrap();
        assert!(rows.iter().any(|r| r["kind"] == "hunk"));
        assert!(rows
            .iter()
            .any(|r| r["new"]["text"] == "TWO" && r["new"]["sign"] == "+"));
        answer(repo, &json!({"op": "stage", "path": "a.txt"}))?;
        // Nothing is unstaged now, so the list opens on the staged view.
        let files = answer(repo, &json!({"op": "files"}))?;
        assert_eq!(files["staged"], true);
        answer(repo, &json!({"op": "unstage", "path": "a.txt"}))?;
        assert_eq!(
            answer(repo, &json!({"op": "files", "staged": true}))?["files"],
            json!([])
        );
        Ok(())
    }
}
