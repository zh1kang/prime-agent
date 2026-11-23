//! The content-addressed digest cache: `<agent_dir>/absorbed/<key>/` holds
//! `digest.json` and `transcript.md` (0600 files in a 0700 directory, written
//! by temp file and rename). `absorbed/sources/<hash>.json` points from an
//! unfocused source file to its latest key, so the history picker can show a
//! cached cost without parsing anything.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest as _, Sha256};

use super::sources::{ResolvedSource, SourceProvider};
use super::{Digest, PROMPT_VERSION, SCHEMA_VERSION};

/// The source file's size and modification time: appending to a chat changes
/// its key, so a re-absorb picks up the new turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub mtime_ns: u128,
    pub size_bytes: u64,
}

impl Fingerprint {
    /// # Errors
    ///
    /// Fails when the source file cannot be read.
    pub fn of(source: &ResolvedSource) -> Result<Self> {
        let meta = std::fs::metadata(&source.path)
            .with_context(|| format!("Missing transcript {}", source.path.display()))?;
        let mtime_ns = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        Ok(Self {
            mtime_ns,
            size_bytes: meta.len(),
        })
    }
}

/// `sha256(schema | prompt | provider | id | path | mtime_ns | size | focus)`.
#[must_use]
pub fn cache_key(
    source: &ResolvedSource,
    fingerprint: &Fingerprint,
    focus: Option<&str>,
) -> String {
    let focus = focus
        .map(|focus| {
            focus
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        })
        .unwrap_or_default();
    let material = format!(
        "absorb\n{SCHEMA_VERSION}\n{PROMPT_VERSION}\n{}\n{}\n{}\n{}\n{}\n{focus}",
        source.provider.as_str(),
        source.thread_id.as_deref().unwrap_or_default(),
        source.path.display(),
        fingerprint.mtime_ns,
        fingerprint.size_bytes,
    );
    hex::encode(Sha256::digest(material.as_bytes()))
}

#[must_use]
pub fn root(agent_dir: &Path) -> PathBuf {
    agent_dir.join("absorbed")
}

#[must_use]
pub fn entry_dir(agent_dir: &Path, key: &str) -> PathBuf {
    root(agent_dir).join(key)
}

/// The pointer file of a source path (`sha256("<provider>\0<path>")`).
#[must_use]
pub fn pointer_path(agent_dir: &Path, provider: SourceProvider, path: &Path) -> PathBuf {
    let material = format!("{}\0{}", provider.as_str(), path.display());
    root(agent_dir).join("sources").join(format!(
        "{}.json",
        hex::encode(Sha256::digest(material.as_bytes()))
    ))
}

/// The cached digest, when present and readable.
#[must_use]
pub fn load(agent_dir: &Path, key: &str) -> Option<Digest> {
    let dir = entry_dir(agent_dir, key);
    let digest: Digest =
        serde_json::from_slice(&std::fs::read(dir.join("digest.json")).ok()?).ok()?;
    (digest.schema == SCHEMA_VERSION
        && digest.prompt_version == PROMPT_VERSION
        && digest.key == key
        && dir.join("transcript.md").is_file())
    .then_some(digest)
}

/// Write the entry (transcript first, so a digest never names a missing
/// transcript) and, for unfocused digests, the source pointer.
///
/// # Errors
///
/// Fails when the cache directory or a file cannot be written.
pub fn store(
    agent_dir: &Path,
    digest: &Digest,
    transcript_md: &str,
    source: &ResolvedSource,
    fingerprint: &Fingerprint,
    unfocused: bool,
) -> Result<()> {
    let dir = entry_dir(agent_dir, &digest.key);
    make_dir(&dir)?;
    write_atomic(&dir.join("transcript.md"), transcript_md.as_bytes())?;
    write_atomic(
        &dir.join("digest.json"),
        &serde_json::to_vec_pretty(digest)?,
    )?;
    if unfocused && source.provider != SourceProvider::T3 {
        let pointer = pointer_path(agent_dir, source.provider, &source.path);
        make_dir(pointer.parent().context("pointer has a parent")?)?;
        let record = serde_json::json!({
            "key": digest.key,
            "mtimeNs": fingerprint.mtime_ns.to_string(),
            "sizeBytes": fingerprint.size_bytes,
            "tokens": digest.tokens.packet,
            "method": digest.method,
        });
        write_atomic(&pointer, record.to_string().as_bytes())?;
    }
    Ok(())
}

fn make_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("Could not create {}", dir.display()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("cache file has a parent")?;
    let temp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("Could not write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_key_tracks_size_mtime_and_focus() {
        let source = ResolvedSource {
            provider: SourceProvider::Claude,
            path: PathBuf::from("/a.jsonl"),
            thread_id: None,
        };
        let fp = Fingerprint {
            mtime_ns: 1,
            size_bytes: 2,
        };
        let base = cache_key(&source, &fp, None);
        assert_eq!(base, cache_key(&source, &fp, None));
        assert_ne!(
            base,
            cache_key(
                &source,
                &Fingerprint {
                    size_bytes: 3,
                    ..fp
                },
                None
            )
        );
        assert_ne!(
            base,
            cache_key(&source, &Fingerprint { mtime_ns: 9, ..fp }, None)
        );
        assert_ne!(base, cache_key(&source, &fp, Some("auth")));
        assert_eq!(
            cache_key(&source, &fp, Some("Auth  bug")),
            cache_key(&source, &fp, Some("auth bug"))
        );
    }

    #[test]
    fn writes_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x");
        write_atomic(&file, b"hi").unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let sub = dir.path().join("a/b");
        make_dir(&sub).unwrap();
        assert_eq!(
            std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
