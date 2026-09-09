use std::path::{Path, PathBuf};

/// What a directory entry is. Symlinks are recorded but not followed: the FUSE
/// volume that backs `/sdcard` cannot hold them, so in practice they only turn
/// up outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// One entry on the device, with its path relative to the transfer root.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    /// Path relative to the scan root. Empty for the root itself.
    pub rel: String,
    /// Absolute path on the device, used verbatim in sync requests.
    pub remote: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: i64,
    /// Permission bits. Synthesized by the FUSE mount for `/sdcard`, so they
    /// carry no information there and are never restored.
    pub mode: u32,
}

/// One entry in the local destination tree.
#[derive(Debug, Clone)]
pub struct LocalEntry {
    pub rel: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: i64,
}

/// Join a relative path onto a local root, rejecting anything that would
/// escape it. Device-supplied paths are untrusted input.
pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() {
        return Some(root.to_path_buf());
    }
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            _ if part.contains('\0') => return None,
            _ => out.push(part),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_relative_paths() {
        let root = Path::new("/tmp/dest");
        assert_eq!(safe_join(root, "a/b.txt").unwrap(), root.join("a/b.txt"));
        assert_eq!(safe_join(root, "").unwrap(), root);
        assert_eq!(safe_join(root, "./a").unwrap(), root.join("a"));
    }

    #[test]
    fn rejects_traversal_and_nul() {
        let root = Path::new("/tmp/dest");
        assert!(safe_join(root, "../etc/passwd").is_none());
        assert!(safe_join(root, "a/../../b").is_none());
        assert!(safe_join(root, "a\0b").is_none());
    }
}
