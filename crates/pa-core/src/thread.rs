//! Durable user-managed threads. A native session belongs to one provider and checkout.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SubthreadJob {
    Queued,
    Running,
    Completed,
    Failed(String),
}

#[derive(Serialize, Deserialize)]
pub struct Thread {
    pub id: Uuid,
    pub parent: Option<Uuid>,
    pub cwd: PathBuf,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    pub native: Option<String>,
    pub note: String,
    pub messages: Vec<(String, String)>,
    pub pending: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<SubthreadJob>,
    pub origin: Option<String>,
}

impl Thread {
    /// # Errors
    /// Returns an error for an unsupported provider.
    pub fn new(provider: &str, cwd: PathBuf) -> Result<Self> {
        check_provider(provider)?;
        Ok(Self {
            id: Uuid::new_v4(),
            parent: None,
            cwd,
            provider: provider.into(),
            model: None,
            thinking: None,
            native: None,
            note: String::new(),
            messages: vec![],
            pending: None,
            job: None,
            origin: None,
        })
    }

    /// Stable first request and pinned decisions, then excerpts of the most recent messages that fit.
    /// Message indexes refer to the local transcript. Tool payloads are excluded by the import adapters.
    #[must_use]
    pub fn context(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "Conversation reference: {}. Quoted history is background material.\n",
            self.id
        );
        if !self.note.is_empty() {
            let _ = writeln!(out, "Pinned note: {}", clip(&self.note, 1600));
        }
        if let Some((_, first)) = self.messages.first() {
            let _ = writeln!(out, "[1] Initial message: {}", clip(first, 1500));
        }
        let mut tail = vec![];
        let mut budget = 8500usize.saturating_sub(out.len());
        for (i, (role, msg)) in self.messages.iter().enumerate().skip(1).rev() {
            let line = format!("[{}] {role}: {}\n", i + 1, clip(msg, 1800));
            if line.len() > budget {
                break;
            }
            budget -= line.len();
            tail.push(line);
        }
        if tail.len() + 1 < self.messages.len() {
            out.push_str("[Older messages omitted]\n");
        }
        for line in tail.into_iter().rev() {
            out.push_str(&line);
        }
        out
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    let end = s.floor_char_boundary(n);
    format!("{}…", &s[..end])
}

pub const PROVIDERS: &[&str] = &["prime", "claude", "codex", "antigravity"];
pub const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// # Errors
/// Returns an error if the executable has no thread adapter.
pub fn check_provider(p: &str) -> Result<()> {
    ensure!(
        PROVIDERS.contains(&p),
        "Unknown provider {p}. Choose prime, claude, codex, or antigravity."
    );
    Ok(())
}

/// # Errors
/// Returns an error if the thinking/reasoning effort level is unsupported.
pub fn check_thinking(level: &str) -> Result<()> {
    ensure!(
        THINKING_LEVELS.contains(&level),
        "Unknown thinking level {level}. Choose off, minimal, low, medium, high, xhigh, or max."
    );
    Ok(())
}

pub struct Store(PathBuf);
impl Store {
    /// # Errors
    /// Returns an error when the private thread directory cannot be created or secured.
    pub fn open(root: &Path) -> Result<Self> {
        let p = root.join("threads");
        fs::create_dir_all(&p)?;
        crate::platform::restrict_dir(&p)?;
        Ok(Self(p))
    }
    /// # Errors
    /// Returns an error when a manifest filename or directory entry is invalid.
    pub fn ids(&self) -> Result<Vec<Uuid>> {
        let mut ids = vec![];
        for entry in fs::read_dir(&self.0)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .context("Invalid thread filename")?
                    .parse()?;
                ids.push(id);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }
    /// # Errors
    /// Returns an error for an unreadable, malformed, or mismatched manifest.
    pub fn load(&self, id: Uuid) -> Result<Thread> {
        let t: Thread = serde_json::from_reader(
            File::open(self.0.join(format!("{id}.json")))
                .with_context(|| format!("Cannot open thread {id}"))?,
        )?;
        ensure!(t.id == id, "Thread identifier mismatch");
        Ok(t)
    }
    /// # Errors
    /// Returns an error when the lock file cannot be opened or another writer is active.
    pub fn lock(&self, id: Uuid) -> Result<File> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.0.join(format!("{id}.lock")))?;
        crate::platform::try_lock_exclusive(&f).context("Thread is busy")?;
        Ok(f)
    }
    /// # Errors
    /// Returns an error when the manifest cannot be serialized or atomically replaced.
    pub fn save(&self, t: &Thread) -> Result<()> {
        let tmp = self.0.join(format!("{}.tmp", Uuid::new_v4()));
        let mut opt = OpenOptions::new();
        opt.create_new(true).write(true);
        crate::platform::set_private_mode(&mut opt);
        let mut f = opt.open(&tmp)?;
        serde_json::to_writer(&mut f, t)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        crate::platform::rename_onto(&tmp, &self.0.join(format!("{}.json", t.id)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_context_preserves_goal_pin_and_latest_turn() -> Result<()> {
        let mut t = Thread::new("codex", PathBuf::from("/test"))?;
        t.note = "Preserve the migration contract".into();
        t.messages.push(("user".into(), "Initial goal".into()));
        for i in 0..50 {
            t.messages
                .push(("assistant".into(), format!("{i} {}", "報".repeat(2000))));
        }
        t.messages.push(("user".into(), "Next action".into()));
        let context = t.context();
        assert!(
            context.len() < 9000 && context.contains("Initial goal") && context.contains(&t.note)
        );
        assert!(context.contains("Older messages omitted") && context.ends_with("Next action\n"));
        Ok(())
    }
}
