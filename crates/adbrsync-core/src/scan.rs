use std::path::{Path, PathBuf};

use adb_proto::{shell, AdbClient, DeviceSelector};

use crate::entry::{EntryKind, LocalEntry, RemoteEntry};
use crate::error::{Error, Result};

/// Outcome of walking the device tree.
#[derive(Debug, Default)]
pub struct RemoteScan {
    /// The scan root after symlink resolution.
    pub root: String,
    pub entries: Vec<RemoteEntry>,
    /// Paths the device refused to let the shell user read. Reported rather
    /// than silently dropped, so a partial backup never looks like a clean one.
    pub denied: Vec<String>,
}

/// Walk the device tree in a single round trip.
///
/// `find -printf` returns the whole tree from one request. The alternative,
/// recursive `LIS2`, pays the same device-side stat cost but needs one round
/// trip per directory; measured on the reference device the device-side walk is
/// 89% of the cost either way, so the extra round trips are pure loss.
///
/// Three device behaviours this has to work around, all of which lose data
/// silently if ignored:
///   1. `/sdcard` is a symlink and `find` does not follow one given as its
///      starting point, so the root is resolved with `readlink -f` first.
///      Without this the walk returns nothing at all.
///   2. `find -exec stat {} +` is unusable: toybox ignores ARG_MAX and fails
///      partway with `Argument list too long`, so `-printf` does the formatting.
///   3. `-printf %y` is unsupported, so the entry type comes from separate
///      `-type` branches with the letter written into the format string.
pub async fn scan_remote(
    client: &AdbClient,
    selector: &DeviceSelector,
    root: &str,
) -> Result<RemoteScan> {
    let out = shell::run(client, selector, &walk_command(root)).await?;

    let mut denied = Vec::new();
    for line in out.stderr_text().lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains("Permission denied") {
            denied.push(strip_find_prefix(line));
        } else {
            // Anything else means the walk may be truncated, and a truncated
            // walk would look exactly like a smaller tree. Refuse to guess.
            return Err(Error::ScanFailed(line.to_string()));
        }
    }
    if out.exit_code != 0 && denied.is_empty() {
        return Err(Error::ScanFailed(format!(
            "device walk exited with status {}",
            out.exit_code
        )));
    }

    let mut records = out.stdout.split(|b| *b == 0);
    let resolved = records
        .next()
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .filter(|r| !r.is_empty())
        .ok_or_else(|| Error::ScanFailed("device walk produced no root".into()))?;

    let mut entries = Vec::new();
    for record in records {
        if record.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(record);
        match parse_record(&text, &resolved) {
            Some(entry) => entries.push(entry),
            None => return Err(Error::ScanFailed(format!("unparsable record {text:?}"))),
        }
    }

    Ok(RemoteScan {
        root: resolved,
        entries,
        denied,
    })
}

fn walk_command(root: &str) -> String {
    let quoted = shell::shell_quote(root);
    format!(
        "R=$(readlink -f {quoted}); \
         if [ -z \"$R\" ] || [ ! -e \"$R\" ]; then echo \"no such path: {root}\" >&2; exit 2; fi; \
         printf '%s\\0' \"$R\"; \
         find \"$R\" \\( -type f -printf 'f|%s|%T@|%m|%p\\0' \\) \
                 -o \\( -type d -printf 'd|%s|%T@|%m|%p\\0' \\) \
                 -o \\( -type l -printf 'l|%s|%T@|%m|%p\\0' \\)",
        root = root.replace('"', "'"),
    )
}

fn parse_record(text: &str, root: &str) -> Option<RemoteEntry> {
    // The path is last because it may itself contain the separator.
    let mut parts = text.splitn(5, '|');
    let kind = match parts.next()? {
        "f" => EntryKind::File,
        "d" => EntryKind::Dir,
        "l" => EntryKind::Symlink,
        _ => return None,
    };
    let size = parts.next()?.parse().ok()?;
    let mtime = parse_mtime(parts.next()?)?;
    let mode = u32::from_str_radix(parts.next()?, 8).ok()?;
    let remote = parts.next()?.to_string();

    // Only strip the root when it really is a path prefix: a plain string
    // prefix would also match a sibling such as `/storage/emulated/0extra`.
    let rel = match remote.strip_prefix(root) {
        Some("") => String::new(),
        Some(rest) if rest.starts_with('/') => rest.trim_start_matches('/').to_string(),
        _ => remote.clone(),
    };

    Some(RemoteEntry {
        rel,
        remote,
        kind,
        size,
        mtime,
        mode,
    })
}

/// `%T@` is seconds with a fractional part; only whole seconds are compared,
/// since local filesystems and the FUSE volume disagree below that.
fn parse_mtime(raw: &str) -> Option<i64> {
    raw.split_once('.')
        .map(|(secs, _)| secs)
        .unwrap_or(raw)
        .parse()
        .ok()
}

fn strip_find_prefix(line: &str) -> String {
    line.strip_prefix("find: ")
        .unwrap_or(line)
        .rsplit_once(": Permission denied")
        .map(|(path, _)| path.trim_matches('\'').to_string())
        .unwrap_or_else(|| line.to_string())
}

/// Walk the local destination tree.
pub fn scan_local(root: &Path) -> Result<Vec<LocalEntry>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for item in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let item = item.map_err(|e| Error::LocalScan {
            path: e.path().unwrap_or(root).to_path_buf(),
            source: e.into(),
        })?;
        let meta = item.metadata().map_err(|e| Error::LocalScan {
            path: item.path().to_path_buf(),
            source: e.into(),
        })?;
        let kind = if meta.is_dir() {
            EntryKind::Dir
        } else if meta.file_type().is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::File
        };
        let rel = item
            .path()
            .strip_prefix(root)
            .unwrap_or(item.path())
            .to_string_lossy()
            .replace('\\', "/");
        entries.push(LocalEntry {
            rel,
            path: item.path().to_path_buf(),
            kind,
            size: meta.len(),
            mtime: mtime_secs(&meta),
        });
    }
    Ok(entries)
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Where a local path lands for a given relative entry.
pub fn local_path_for(dest: &Path, rel: &str) -> Option<PathBuf> {
    crate::entry::safe_join(dest, rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_file_record() {
        let e = parse_record(
            "f|1234|1730772582.354874934|660|/storage/emulated/0/DCIM/a.jpg",
            "/storage/emulated/0",
        )
        .unwrap();
        assert_eq!(e.kind, EntryKind::File);
        assert_eq!(e.size, 1234);
        assert_eq!(e.mtime, 1_730_772_582);
        assert_eq!(e.mode, 0o660);
        assert_eq!(e.rel, "DCIM/a.jpg");
        assert_eq!(e.remote, "/storage/emulated/0/DCIM/a.jpg");
    }

    #[test]
    fn keeps_pipes_that_belong_to_the_filename() {
        let e = parse_record(
            "f|10|1700000000|644|/storage/emulated/0/we|ird|name.txt",
            "/storage/emulated/0",
        )
        .unwrap();
        assert_eq!(e.rel, "we|ird|name.txt");
    }

    #[test]
    fn does_not_strip_a_root_that_is_only_a_string_prefix() {
        let e = parse_record(
            "f|1|1700000000|644|/storage/emulated/0extra/a.txt",
            "/storage/emulated/0",
        )
        .unwrap();
        assert_eq!(e.rel, "/storage/emulated/0extra/a.txt");
    }

    #[test]
    fn root_itself_has_an_empty_relative_path() {
        let e = parse_record(
            "d|4096|1700000000|770|/storage/emulated/0",
            "/storage/emulated/0",
        )
        .unwrap();
        assert_eq!(e.rel, "");
        assert_eq!(e.kind, EntryKind::Dir);
    }

    #[test]
    fn accepts_mtime_without_a_fraction() {
        assert_eq!(parse_mtime("1700000000"), Some(1_700_000_000));
        assert_eq!(parse_mtime("1700000000.5"), Some(1_700_000_000));
        assert_eq!(parse_mtime("nope"), None);
    }

    #[test]
    fn rejects_malformed_records() {
        assert!(parse_record("x|1|2|3|/p", "/").is_none());
        assert!(parse_record("f|notanumber|2|3|/p", "/").is_none());
    }

    #[test]
    fn extracts_the_denied_path_from_a_find_error() {
        assert_eq!(
            strip_find_prefix("find: '/storage/emulated/0/Android/data': Permission denied"),
            "/storage/emulated/0/Android/data"
        );
    }

    #[test]
    fn walk_command_resolves_the_root_symlink() {
        let cmd = walk_command("/sdcard");
        assert!(cmd.contains("readlink -f '/sdcard'"));
        assert!(cmd.contains("find \"$R\""));
        // The ARG_MAX trap must not reappear.
        assert!(!cmd.contains("-exec"));
    }
}
