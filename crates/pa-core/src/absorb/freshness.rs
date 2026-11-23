//! The absorb-time workspace check: which of the source chat's files changed
//! since it ended. It runs on every absorb (never cached in the digest) and
//! never fails: a missing worktree or another repository becomes a note.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{format_utc, Digest};

/// Changed files listed before the rest collapse into a count.
const MAX_LINES: usize = 12;
/// Files checked at most.
const MAX_FILES: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Freshness {
    pub checked_ms: u64,
    pub notes: Vec<String>,
    /// One line per changed file, sorted.
    pub lines: Vec<String>,
    pub changed_files: Vec<String>,
    pub unchanged: usize,
}

impl Freshness {
    /// The packet section, under ~150 tokens.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "## Workspace since the source chat (checked {})\n",
            format_utc(self.checked_ms)
        );
        for note in &self.notes {
            out.push_str(&format!("- {note}\n"));
        }
        for line in self.lines.iter().take(MAX_LINES) {
            out.push_str(&format!("- {line}\n"));
        }
        if self.lines.len() > MAX_LINES {
            out.push_str(&format!(
                "- … and {} more changed\n",
                self.lines.len() - MAX_LINES
            ));
        }
        if self.unchanged > 0 {
            out.push_str(&format!("- {} files unchanged\n", self.unchanged));
        }
        if self.notes.is_empty() && self.lines.is_empty() && self.unchanged == 0 {
            out.push_str("- no file state to check\n");
        }
        out.trim_end().to_owned()
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn toplevel(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"]).map(|out| super::canonical(Path::new(out.trim())))
}

/// Compare the digest's files against the live workspace at `cwd`.
#[must_use]
pub fn check(digest: &Digest, cwd: &Path, now_ms: u64) -> Freshness {
    let mut out = Freshness {
        checked_ms: now_ms,
        ..Freshness::default()
    };
    let source_cwd = digest.source.cwd.as_deref().map(PathBuf::from);
    let files: BTreeSet<&str> = digest
        .files
        .modified
        .iter()
        .chain(&digest.files.read)
        .map(String::as_str)
        .take(MAX_FILES)
        .collect();
    let absolute = |file: &str| -> PathBuf {
        let path = Path::new(file);
        match (&source_cwd, path.is_absolute()) {
            (Some(base), false) => base.join(path),
            _ => path.to_path_buf(),
        }
    };
    let current_top = toplevel(cwd);
    let source_top = match &source_cwd {
        Some(dir) if dir.is_dir() => toplevel(dir),
        Some(dir) => {
            out.notes.push(format!(
                "Source chat ran in {} (no longer present); paths refer to that tree.",
                dir.display()
            ));
            None
        }
        None => None,
    };
    match (&current_top, &source_top) {
        (Some(current), Some(source)) if current == source => {
            git_check(&mut out, current, digest, &files, &absolute);
            return out;
        }
        (_, Some(source)) => out.notes.push(format!(
            "Source chat ran in {}; file paths refer to that tree.",
            source.display()
        )),
        (_, None) => {
            if let Some(dir) = source_cwd
                .as_ref()
                .filter(|dir| dir.is_dir() && super::canonical(dir) != super::canonical(cwd))
            {
                out.notes.push(format!(
                    "Source chat ran in {}; file paths refer to that tree.",
                    dir.display()
                ));
            }
        }
    }
    mtime_check(&mut out, digest.source.ended_ms, &files, &absolute);
    out
}

fn git_check(
    out: &mut Freshness,
    top: &Path,
    digest: &Digest,
    files: &BTreeSet<&str>,
    absolute: &dyn Fn(&str) -> PathBuf,
) {
    if let (Some(then), Some(now)) = (
        digest.source.git_branch.as_deref(),
        git(top, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|b| b.trim().to_owned()),
    ) {
        if then != now {
            out.notes
                .push(format!("branch now `{now}` (source chat was on `{then}`)"));
        }
    }
    // Pathspecs relative to the top level; files outside it fall back to mtime.
    let mut inside: Vec<(String, String)> = Vec::new();
    let mut outside: BTreeSet<&str> = BTreeSet::new();
    for file in files {
        let path = super::canonical(&absolute(file));
        match path.strip_prefix(top) {
            Ok(rel) => inside.push(((*file).to_owned(), rel.to_string_lossy().into_owned())),
            Err(_) => {
                outside.insert(file);
            }
        }
    }
    if !inside.is_empty() {
        let mut status_args = vec!["status", "--porcelain", "--"];
        status_args.extend(inside.iter().map(|(_, rel)| rel.as_str()));
        let dirty: BTreeSet<String> = git(top, &status_args)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.get(3..))
            .map(|path| {
                path.rsplit(" -> ")
                    .next()
                    .unwrap_or(path)
                    .trim_matches('"')
                    .to_owned()
            })
            .collect();
        let since = format!("--since=@{}", digest.source.ended_ms / 1000);
        let mut log_args = vec!["log", "--format=", "--name-only", since.as_str(), "--"];
        log_args.extend(inside.iter().map(|(_, rel)| rel.as_str()));
        let mut committed: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for line in git(top, &log_args)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
        {
            *committed.entry(line.to_owned()).or_default() += 1;
        }
        for (shown, rel) in &inside {
            let exists = top.join(rel).exists();
            let mut reasons = Vec::new();
            if !exists {
                reasons.push("deleted or moved".to_owned());
            } else if dirty.contains(rel) {
                reasons.push("uncommitted changes".to_owned());
            }
            if let Some(count) = committed.get(rel) {
                reasons.push(format!("{count} commit(s) since the chat"));
            }
            if reasons.is_empty() {
                out.unchanged += 1;
            } else {
                out.lines.push(format!("{shown}: {}", reasons.join("; ")));
                out.changed_files.push((*shown).to_owned());
            }
        }
    }
    mtime_check(out, digest.source.ended_ms, &outside, absolute);
}

fn mtime_check(
    out: &mut Freshness,
    ended_ms: u64,
    files: &BTreeSet<&str>,
    absolute: &dyn Fn(&str) -> PathBuf,
) {
    for file in files {
        let path = absolute(file);
        let line = match std::fs::metadata(&path) {
            Err(_) => Some("deleted or moved"),
            Ok(meta) => {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |since| since.as_millis() as u64);
                (modified > ended_ms).then_some("modified after the chat ended")
            }
        };
        match line {
            Some(reason) => {
                out.lines.push(format!("{file}: {reason}"));
                out.changed_files.push((*file).to_owned());
            }
            None => out.unchanged += 1,
        }
    }
    out.lines.sort();
    out.changed_files.sort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::absorb::{DigestFiles, DigestSource, DigestTokens, PROMPT_VERSION, SCHEMA_VERSION};

    pub(crate) fn digest_for(
        cwd: &Path,
        ended_ms: u64,
        modified: &[&str],
        read: &[&str],
    ) -> Digest {
        Digest {
            schema: SCHEMA_VERSION,
            prompt_version: PROMPT_VERSION,
            key: "k".into(),
            method: "verbatim".into(),
            model: None,
            source: DigestSource {
                provider: "claude".into(),
                id: "c".into(),
                path: "/x.jsonl".into(),
                title: "t".into(),
                cwd: Some(cwd.display().to_string()),
                git_branch: None,
                head_commit: None,
                started_ms: ended_ms,
                ended_ms,
                turns: 1,
                raw_bytes: 1,
                raw_text_tokens: 1,
            },
            files: DigestFiles {
                modified: modified.iter().map(|s| (*s).to_owned()).collect(),
                read: read.iter().map(|s| (*s).to_owned()).collect(),
                known: true,
            },
            commands: Vec::new(),
            commits: Vec::new(),
            summary_md: String::new(),
            card_md: String::new(),
            transcript: "/t.md".into(),
            tokens: DigestTokens {
                packet: 1,
                layer0: 1,
                raw_text: 1,
            },
            warnings: Vec::new(),
        }
    }

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn flags_a_modified_file_in_the_same_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = super::super::canonical(dir.path());
        run(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.rs"), "one").unwrap();
        std::fs::write(repo.join("b.rs"), "one").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);
        std::fs::write(repo.join("a.rs"), "two").unwrap();
        let now = 4_000_000_000_000;
        let fresh = check(
            &digest_for(&repo, now, &["a.rs"], &["b.rs", "gone.rs"]),
            &repo,
            now,
        );
        assert_eq!(fresh.changed_files, ["a.rs", "gone.rs"]);
        assert_eq!(fresh.unchanged, 1);
        let text = fresh.render();
        assert!(text.contains("a.rs: uncommitted changes"), "{text}");
        assert!(text.contains("gone.rs: deleted or moved"), "{text}");
        assert!(text.contains("1 files unchanged"), "{text}");
    }

    #[test]
    fn a_deleted_worktree_is_a_note_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("removed-worktree");
        let fresh = check(&digest_for(&gone, 1, &["src/x.rs"], &[]), dir.path(), 2);
        assert!(fresh.notes[0].contains("no longer present"));
        assert_eq!(fresh.changed_files, ["src/x.rs"]);
        assert!(fresh.render().contains("src/x.rs: deleted or moved"));
    }
}
