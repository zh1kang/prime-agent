//! Git operations scoped to the working directory recorded for a thread.

use std::path::{Path, PathBuf};
use std::process::Command;

mod review_data;

use anyhow::{bail, ensure, Context, Result};

fn git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "--no-pager",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
        ])
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "");
    // A thread always acts on its own checkout, even if launched by a Git hook.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_PREFIX",
    ] {
        command.env_remove(key);
    }
    command
}

fn run(command: &mut Command) -> Result<()> {
    let status = command.status().context("Could not start git")?;
    ensure!(status.success(), "git returned {status}");
    Ok(())
}

fn git_dir(cwd: &Path, flag: &str) -> Result<PathBuf> {
    let output = git(cwd)
        .args(["rev-parse", "--path-format=absolute", flag])
        .output()?;
    ensure!(
        output.status.success(),
        "Could not identify repository at {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr).trim_end()
    );
    let value = std::str::from_utf8(&output.stdout)?
        .strip_suffix('\n')
        .context("Missing Git path")?;
    #[cfg(windows)]
    let value = value.strip_suffix('\r').unwrap_or(value);
    Ok(Path::new(value).canonicalize()?)
}

/// Returns the new thread cwd only after Git confirms the target is a checkout
/// of this repository. Other sessions and their working trees remain independent.
pub(crate) fn worktree(cwd: &Path, args: &[String]) -> Result<Option<PathBuf>> {
    match args {
        [] => run(git(cwd).args(["worktree", "list"]))?,
        [action] if action == "list" => run(git(cwd).args(["worktree", "list"]))?,
        [action, dest] if action == "move" => {
            let dest = cwd
                .join(dest)
                .canonicalize()
                .context("Worktree path does not exist")?;
            ensure!(
                git_dir(&dest, "--show-toplevel")? == dest
                    && git_dir(&dest, "--git-common-dir")? == git_dir(cwd, "--git-common-dir")?,
                "Select the root of a worktree belonging to this repository"
            );
            return Ok(Some(dest));
        }
        [action, dest, branch] if action == "create" => {
            ensure!(!dest.is_empty(), "Specify a worktree path");
            run(git(cwd).args(["check-ref-format", "--branch", branch]))?;
            run(git(cwd).args(["worktree", "add", "-b", branch, "--", dest, "HEAD"]))?;
            return Ok(Some(cwd.join(dest).canonicalize()?));
        }
        _ => bail!("worktree list | move <path> | create <path> <new-branch>"),
    }
    Ok(None)
}

pub(crate) fn review(cwd: &Path, args: &[String]) -> Result<()> {
    if matches!(args, [action] if action == "json") {
        // pa's native review panel: one JSON request on stdin, one JSON answer on stdout.
        let request: serde_json::Value = serde_json::from_reader(std::io::stdin().lock())
            .context("Send one JSON review request on stdin")?;
        let answer = review_data::answer(&git_dir(cwd, "--show-toplevel")?, &request)?;
        println!("{answer}");
        return Ok(());
    }
    if matches!(args, [action] if action == "gui") {
        let root = git_dir(cwd, "--show-toplevel")?;
        #[cfg(target_os = "macos")]
        {
            return run(Command::new("/usr/bin/open")
                .args(["-n", "-a", "Diffy", "--args", "--repo"])
                .arg(root))
            .context("Could not open Diffy for this worktree");
        }
        #[cfg(not(target_os = "macos"))]
        {
            Command::new("diffy")
                .arg("--repo")
                .arg(root)
                .spawn()
                .context("Could not start diffy; install Diffy or use review diff")?;
            return Ok(());
        }
    }
    let mut command = git(cwd);
    let (action, paths) = args
        .split_first()
        .map_or(("status", &[][..]), |(s, p)| (s.as_str(), p));
    match action {
        "status" => command.args(["status", "--short", "--branch"]),
        "diff" => command.args(["diff", "--no-ext-diff", "--no-textconv"]),
        "staged" => command.args(["diff", "--cached", "--no-ext-diff", "--no-textconv"]),
        "stage" | "unstage" if paths.is_empty() || paths.iter().any(String::is_empty) => {
            bail!("Specify the paths to {action}")
        }
        "stage" => command.arg("add"),
        "unstage" => command.args(["reset", "--quiet"]),
        _ => bail!("review status | diff | staged | stage <path>... | unstage <path>..."),
    };
    run(command.arg("--").args(paths))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn literal_staging_and_same_repository_moves() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo)?;
        run(git(&repo).args(["init", "-q"]))?;
        run(git(&repo).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
            "--allow-empty",
        ]))?;
        std::fs::write(repo.join("*.txt"), "one")?;
        std::fs::write(repo.join("other.txt"), "two")?;
        review(&repo, &words(&["stage", "*.txt"]))?;
        assert_eq!(
            git(&repo)
                .args(["diff", "--cached", "--name-only"])
                .output()?
                .stdout,
            b"*.txt\n"
        );
        review(&repo, &words(&["unstage", "*.txt"]))?;
        assert!(git(&repo)
            .args(["diff", "--cached", "--name-only"])
            .output()?
            .stdout
            .is_empty());
        let checkout = worktree(&repo, &words(&["create", "../second", "second"]))?;
        assert_eq!(worktree(&repo, &words(&["move", "../second"]))?, checkout);
        run(git(temp.path()).args(["init", "-q", "other"]))?;
        assert!(worktree(&repo, &words(&["move", "../other"])).is_err());
        Ok(())
    }
}
