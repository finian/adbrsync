use std::collections::{HashMap, HashSet};
use std::path::Path;

use adb_proto::{shell, AdbClient, DeviceSelector};
use sha2::{Digest, Sha256};

use crate::entry::{EntryKind, LocalEntry, RemoteEntry};
use crate::error::{Error, Result};

/// Relative paths whose device and local contents hash identically.
#[derive(Debug, Default)]
pub struct Digests {
    pub matched: HashSet<String>,
}

impl Digests {
    pub fn matches(&self, rel: &str) -> bool {
        self.matched.contains(rel)
    }
}

/// Keep each batched command well under any plausible ARG_MAX.
///
/// This is deliberate: `find -exec ... {} +` on the reference device ignores
/// ARG_MAX and fails partway with `Argument list too long`, so batching is done
/// here where the size is known rather than delegated to the device.
const MAX_COMMAND_BYTES: usize = 16 * 1024;

/// Compare device and local files by content rather than size and mtime.
///
/// Only files present on both sides are considered; anything missing locally is
/// going to be transferred regardless.
pub async fn compare(
    client: &AdbClient,
    selector: &DeviceSelector,
    remote: &[RemoteEntry],
    local: &[LocalEntry],
    dest: &Path,
) -> Result<Digests> {
    let local_by_rel: HashMap<&str, &LocalEntry> = local
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .map(|e| (e.rel.as_str(), e))
        .collect();

    let candidates: Vec<&RemoteEntry> = remote
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .filter(|e| local_by_rel.contains_key(e.rel.as_str()))
        .collect();
    if candidates.is_empty() {
        return Ok(Digests::default());
    }

    let remote_digests = device_digests(
        client,
        selector,
        &candidates
            .iter()
            .map(|e| e.remote.clone())
            .collect::<Vec<_>>(),
    )
    .await?;

    let paths: Vec<(String, std::path::PathBuf)> = candidates
        .iter()
        .filter_map(|e| crate::entry::safe_join(dest, &e.rel).map(|p| (e.remote.clone(), p)))
        .collect();
    let local_digests = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .filter_map(|(key, path)| local_digest(&path).ok().map(|d| (key, d)))
            .collect::<HashMap<String, String>>()
    })
    .await
    .map_err(|e| Error::Io(std::io::Error::other(e)))?;

    let mut matched = HashSet::new();
    for entry in candidates {
        let (Some(a), Some(b)) = (
            remote_digests.get(&entry.remote),
            local_digests.get(&entry.remote),
        ) else {
            // A digest we could not obtain means "assume different", which
            // costs a transfer but never silently keeps a stale file.
            continue;
        };
        if a == b {
            matched.insert(entry.rel.clone());
        }
    }
    Ok(Digests { matched })
}

/// Hash files on the device, batched into commands of a bounded size.
async fn device_digests(
    client: &AdbClient,
    selector: &DeviceSelector,
    paths: &[String],
) -> Result<HashMap<String, String>> {
    let mut digests = HashMap::new();
    let mut batch: Vec<&String> = Vec::new();
    let mut batch_bytes = 0usize;

    for path in paths {
        let quoted_len = path.len() + 4;
        if !batch.is_empty() && batch_bytes + quoted_len > MAX_COMMAND_BYTES {
            run_batch(client, selector, &batch, &mut digests).await?;
            batch.clear();
            batch_bytes = 0;
        }
        batch.push(path);
        batch_bytes += quoted_len;
    }
    if !batch.is_empty() {
        run_batch(client, selector, &batch, &mut digests).await?;
    }
    Ok(digests)
}

async fn run_batch(
    client: &AdbClient,
    selector: &DeviceSelector,
    batch: &[&String],
    out: &mut HashMap<String, String>,
) -> Result<()> {
    let mut command = String::from("sha256sum");
    for path in batch {
        command.push(' ');
        command.push_str(&shell::shell_quote(path));
    }
    let result = shell::run(client, selector, &command).await?;
    for (path, digest) in parse_sha256sum(&result.stdout_text()) {
        out.insert(path, digest);
    }
    Ok(())
}

/// Parse `sha256sum` output lines of the form `<hex>  <path>`.
///
/// Unparsable lines are dropped rather than guessed at; the caller treats a
/// missing digest as "different".
fn parse_sha256sum(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (digest, path) = line.split_once("  ")?;
            let digest = digest.trim();
            (digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| (path.to_string(), digest.to_ascii_lowercase()))
        })
        .collect()
}

/// Hash a local file.
pub fn local_digest(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sha256sum_lines() {
        let text = "\
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  /sdcard/a.txt
0000000000000000000000000000000000000000000000000000000000000001  /sdcard/b c.txt
";
        let parsed = parse_sha256sum(text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "/sdcard/a.txt");
        assert_eq!(parsed[0].1.len(), 64);
        assert_eq!(parsed[1].0, "/sdcard/b c.txt");
    }

    #[test]
    fn drops_lines_that_are_not_digests() {
        let text = "sha256sum: /sdcard/x: Permission denied\nnot a digest  /sdcard/y\n";
        assert!(parse_sha256sum(text).is_empty());
    }

    #[test]
    fn hashes_a_local_file() {
        let dir = std::env::temp_dir().join("adbrsync-digest-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            local_digest(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
