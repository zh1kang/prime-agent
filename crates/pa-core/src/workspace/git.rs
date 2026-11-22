//! The workspace module's one git entry: every invocation runs with prompts disabled, no
//! fsmonitor hook, no lazy promisor fetch, a deadline, and capped output.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::platform::command_output::{output_with_timeout, BoundedOutput};

/// Deadline for one git invocation (worktree checkouts of large repositories included).
const GIT_TIMEOUT: Duration = Duration::from_mins(5);
/// Output cap per stream; diff callers apply their own smaller byte limits on top.
const GIT_MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;

/// Command-line overrides every invocation starts with: `-c` keys outrank repository config.
const GIT_OVERRIDES: [&str; 4] = ["-c", "core.fsmonitor=false", "-c", "core.quotepath=false"];

/// One git invocation in `cwd`. Extra environment pairs (a private index file, a fixed
/// identity for checkpoint objects) apply on top of the safety environment.
pub(crate) fn run_git_with_env(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &std::ffi::OsStr)],
) -> Result<BoundedOutput> {
    let mut command = Command::new("git");
    command
        .args(GIT_OVERRIDES)
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_EDITOR", "true")
        .env("LC_ALL", "C");
    for (key, value) in env {
        command.env(key, value);
    }
    let output = output_with_timeout(&mut command, GIT_TIMEOUT, GIT_MAX_STREAM_BYTES)
        .map_err(|error| anyhow!("Could not run git: {error}"))?;
    if output.timed_out {
        return Err(anyhow!("git {} timed out", args.first().unwrap_or(&"")));
    }
    Ok(output)
}

/// [`run_git_with_env`] without extra environment.
pub(crate) fn run_git(cwd: &Path, args: &[&str]) -> Result<BoundedOutput> {
    run_git_with_env(cwd, args, &[])
}

/// Run git and require exit 0; the error carries git's own message.
pub(crate) fn git_ok(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = run_git(cwd, args)?;
    if !output.succeeded() {
        let stderr = output.stderr_text();
        let detail = if stderr.is_empty() {
            format!("exit {}", output.status.unwrap_or(-1))
        } else {
            stderr
        };
        return Err(anyhow!("git {} failed: {detail}", args.join(" ")));
    }
    Ok(output.stdout_text())
}

/// The repository identity of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoInfo {
    /// The checkout root that contains `cwd` (a linked worktree's own root inside one).
    pub toplevel: PathBuf,
    /// The shared git dir every worktree of the repository points at.
    pub common_dir: PathBuf,
}

/// `None` outside a git work tree.
pub(crate) fn repo_info(cwd: &Path) -> Option<RepoInfo> {
    let output = run_git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ],
    )
    .ok()?;
    if !output.succeeded() {
        return None;
    }
    let text = output.stdout_text();
    let mut lines = text.lines();
    let toplevel = PathBuf::from(lines.next()?.trim());
    let common_dir = PathBuf::from(lines.next()?.trim());
    Some(RepoInfo {
        toplevel,
        common_dir,
    })
}

/// The main checkout of a repository: the parent of the common `.git` dir (a bare common dir
/// has none, so the toplevel stands in).
pub(crate) fn main_checkout(info: &RepoInfo) -> PathBuf {
    if info.common_dir.file_name().is_some_and(|name| name == ".git") {
        if let Some(parent) = info.common_dir.parent() {
            return parent.to_path_buf();
        }
    }
    info.toplevel.clone()
}

/// The current branch name; `None` on a detached HEAD.
pub(crate) fn current_branch(cwd: &Path) -> Option<String> {
    git_ok(cwd, &["branch", "--show-current"])
        .ok()
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty())
}

/// The commit a revision names, if any.
pub(crate) fn resolve_commit(cwd: &Path, revision: &str) -> Option<String> {
    let spec = format!("{revision}^{{commit}}");
    git_ok(cwd, &["rev-parse", "--verify", "--quiet", "--end-of-options", &spec])
        .ok()
        .map(|oid| oid.trim().to_string())
        .filter(|oid| !oid.is_empty())
}

/// Reject revisions git would parse as options.
pub(crate) fn refuse_option_like(value: &str, what: &str) -> Result<()> {
    if value.starts_with('-') {
        return Err(anyhow!("{what} '{value}' must not start with '-'"));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_repo {
    //! Throwaway repositories for the workspace tests (the only place these tests set a git
    //! identity: it never reaches this project's history).
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub(crate) fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A repository with one commit on `main` holding `README.md` and `src/lib.txt`.
    pub(crate) fn init(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        std::fs::write(repo.join("src/lib.txt"), "one\ntwo\nthree\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "--quiet", "-m", "initial"]);
        std::fs::canonicalize(repo).unwrap()
    }
}
