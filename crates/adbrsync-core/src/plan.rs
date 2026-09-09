use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::checksum::Digests;
use crate::entry::{EntryKind, LocalEntry, RemoteEntry};
use crate::filter::Filter;
use crate::scan::RemoteScan;

#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// Remove local entries that no longer exist on the device.
    pub delete: bool,
    /// Treat mtimes within this many seconds as equal. The FUSE volume and
    /// local filesystems do not agree below one second.
    pub mtime_tolerance: i64,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Compare by content instead of size and mtime.
    pub checksum: bool,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            delete: false,
            mtime_tolerance: 1,
            min_size: None,
            max_size: None,
            checksum: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransferItem {
    pub rel: String,
    pub remote: String,
    pub size: u64,
    pub mtime: i64,
}

/// Why a file is being transferred, for `-v` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Missing,
    SizeDiffers,
    TimeDiffers,
    ContentDiffers,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Relative directories that must exist before transferring.
    pub dirs: Vec<String>,
    /// Files to transfer, largest first.
    pub transfers: Vec<TransferItem>,
    pub reasons: HashMap<String, Reason>,
    /// Local paths to remove, deepest first so directories empty out.
    pub deletions: Vec<PathBuf>,
    pub unchanged: usize,
    pub filtered: usize,
    /// Symlinks on the device, which v0 does not reproduce.
    pub symlinks_skipped: usize,
    pub total_bytes: u64,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.transfers.is_empty() && self.deletions.is_empty() && self.dirs.is_empty()
    }
}

/// Compare the two trees and decide the work.
///
/// The default comparison is size plus mtime, which is rsync's quick check.
/// The rolling-checksum delta algorithm is deliberately absent: Android backup
/// corpora are dominated by immutable media, so it would force a full
/// device-side read of every candidate to save transfers that do not happen.
pub fn build(
    remote: &RemoteScan,
    local: &[LocalEntry],
    dest: &Path,
    filter: &Filter,
    opts: &PlanOptions,
    digests: &Digests,
) -> Plan {
    let local_by_rel: HashMap<&str, &LocalEntry> =
        local.iter().map(|e| (e.rel.as_str(), e)).collect();

    let mut plan = Plan::default();
    let mut wanted: HashSet<&str> = HashSet::new();

    for entry in &remote.entries {
        if entry.rel.is_empty() {
            continue; // the root itself
        }
        if !filter.accepts(&entry.rel) {
            plan.filtered += 1;
            continue;
        }
        wanted.insert(entry.rel.as_str());

        match entry.kind {
            EntryKind::Dir => plan.dirs.push(entry.rel.clone()),
            EntryKind::Symlink => plan.symlinks_skipped += 1,
            EntryKind::File => {
                if let Some(min) = opts.min_size {
                    if entry.size < min {
                        plan.filtered += 1;
                        continue;
                    }
                }
                if let Some(max) = opts.max_size {
                    if entry.size > max {
                        plan.filtered += 1;
                        continue;
                    }
                }
                match classify(
                    entry,
                    local_by_rel.get(entry.rel.as_str()).copied(),
                    opts,
                    digests,
                ) {
                    Some(reason) => {
                        plan.reasons.insert(entry.rel.clone(), reason);
                        plan.total_bytes += entry.size;
                        plan.transfers.push(TransferItem {
                            rel: entry.rel.clone(),
                            remote: entry.remote.clone(),
                            size: entry.size,
                            mtime: entry.mtime,
                        });
                    }
                    None => plan.unchanged += 1,
                }
            }
        }
    }

    // Largest first, so the long tail of small files overlaps with the big
    // transfers instead of trailing after them as a latency-bound trickle.
    plan.transfers.sort_by_key(|t| std::cmp::Reverse(t.size));
    plan.dirs.sort();

    if opts.delete {
        let mut deletions: Vec<&LocalEntry> = local
            .iter()
            .filter(|e| !wanted.contains(e.rel.as_str()))
            // Excluded paths are protected from deletion, as in rsync.
            .filter(|e| filter.accepts(&e.rel))
            .collect();
        // Deepest first so a directory is empty by the time it is removed.
        deletions.sort_by_key(|e| std::cmp::Reverse(e.rel.len()));
        plan.deletions = deletions.into_iter().map(|e| dest.join(&e.rel)).collect();
    }

    plan
}

fn classify(
    remote: &RemoteEntry,
    local: Option<&LocalEntry>,
    opts: &PlanOptions,
    digests: &Digests,
) -> Option<Reason> {
    let Some(local) = local else {
        return Some(Reason::Missing);
    };
    if local.kind != EntryKind::File {
        return Some(Reason::Missing);
    }
    if opts.checksum {
        // Content is the only authority in this mode; size and mtime are not
        // consulted at all, as in rsync -c.
        return (!digests.matches(&remote.rel)).then_some(Reason::ContentDiffers);
    }
    if local.size != remote.size {
        return Some(Reason::SizeDiffers);
    }
    if (local.mtime - remote.mtime).abs() > opts.mtime_tolerance {
        return Some(Reason::TimeDiffers);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{EntryKind, LocalEntry, RemoteEntry};

    fn remote_file(rel: &str, size: u64, mtime: i64) -> RemoteEntry {
        RemoteEntry {
            rel: rel.into(),
            remote: format!("/root/{rel}"),
            kind: EntryKind::File,
            size,
            mtime,
            mode: 0o644,
        }
    }

    fn local_file(rel: &str, size: u64, mtime: i64) -> LocalEntry {
        LocalEntry {
            rel: rel.into(),
            path: PathBuf::from(format!("/dest/{rel}")),
            kind: EntryKind::File,
            size,
            mtime,
        }
    }

    fn scan(entries: Vec<RemoteEntry>) -> RemoteScan {
        RemoteScan {
            root: "/root".into(),
            entries,
            denied: vec![],
        }
    }

    fn plan_of(remote: Vec<RemoteEntry>, local: Vec<LocalEntry>, opts: PlanOptions) -> Plan {
        build(
            &scan(remote),
            &local,
            Path::new("/dest"),
            &Filter::default(),
            &opts,
            &Digests::default(),
        )
    }

    #[test]
    fn transfers_missing_files() {
        let p = plan_of(
            vec![remote_file("a", 10, 100)],
            vec![],
            PlanOptions::default(),
        );
        assert_eq!(p.transfers.len(), 1);
        assert_eq!(p.reasons["a"], Reason::Missing);
        assert_eq!(p.total_bytes, 10);
    }

    #[test]
    fn skips_files_matching_on_size_and_time() {
        let p = plan_of(
            vec![remote_file("a", 10, 100)],
            vec![local_file("a", 10, 100)],
            PlanOptions::default(),
        );
        assert!(p.transfers.is_empty());
        assert_eq!(p.unchanged, 1);
    }

    #[test]
    fn tolerates_sub_second_mtime_drift() {
        let p = plan_of(
            vec![remote_file("a", 10, 100)],
            vec![local_file("a", 10, 101)],
            PlanOptions::default(),
        );
        assert_eq!(p.unchanged, 1);

        let p = plan_of(
            vec![remote_file("a", 10, 100)],
            vec![local_file("a", 10, 105)],
            PlanOptions::default(),
        );
        assert_eq!(p.reasons["a"], Reason::TimeDiffers);
    }

    #[test]
    fn detects_size_change_even_when_time_matches() {
        let p = plan_of(
            vec![remote_file("a", 11, 100)],
            vec![local_file("a", 10, 100)],
            PlanOptions::default(),
        );
        assert_eq!(p.reasons["a"], Reason::SizeDiffers);
    }

    #[test]
    fn orders_transfers_largest_first() {
        let p = plan_of(
            vec![
                remote_file("small", 1, 0),
                remote_file("big", 1000, 0),
                remote_file("mid", 50, 0),
            ],
            vec![],
            PlanOptions::default(),
        );
        let order: Vec<_> = p.transfers.iter().map(|t| t.rel.as_str()).collect();
        assert_eq!(order, ["big", "mid", "small"]);
    }

    #[test]
    fn delete_removes_only_extraneous_entries_deepest_first() {
        let opts = PlanOptions {
            delete: true,
            ..Default::default()
        };
        let p = plan_of(
            vec![remote_file("keep", 1, 0)],
            vec![
                local_file("keep", 1, 0),
                local_file("gone", 1, 0),
                local_file("deep/nested/gone", 1, 0),
            ],
            opts,
        );
        assert_eq!(
            p.deletions,
            vec![
                PathBuf::from("/dest/deep/nested/gone"),
                PathBuf::from("/dest/gone")
            ]
        );
    }

    #[test]
    fn size_bounds_filter_files() {
        let opts = PlanOptions {
            min_size: Some(10),
            max_size: Some(100),
            ..Default::default()
        };
        let p = plan_of(
            vec![
                remote_file("tiny", 5, 0),
                remote_file("ok", 50, 0),
                remote_file("huge", 500, 0),
            ],
            vec![],
            opts,
        );
        assert_eq!(p.transfers.len(), 1);
        assert_eq!(p.transfers[0].rel, "ok");
        assert_eq!(p.filtered, 2);
    }

    #[test]
    fn checksum_mode_ignores_size_and_time() {
        let opts = PlanOptions {
            checksum: true,
            ..Default::default()
        };
        let mut digests = Digests::default();
        digests.matched.insert("same".to_string());

        // Sizes and times differ, but the content matches: no transfer.
        let p = build(
            &scan(vec![remote_file("same", 10, 100)]),
            &[local_file("same", 999, 999)],
            Path::new("/dest"),
            &Filter::default(),
            &opts,
            &digests,
        );
        assert_eq!(p.unchanged, 1);
        assert!(p.transfers.is_empty());

        // Size and time match, but the content does not: transfer.
        let p = build(
            &scan(vec![remote_file("other", 10, 100)]),
            &[local_file("other", 10, 100)],
            Path::new("/dest"),
            &Filter::default(),
            &opts,
            &digests,
        );
        assert_eq!(p.reasons["other"], Reason::ContentDiffers);
    }

    #[test]
    fn excluded_local_files_are_protected_from_deletion() {
        let filter = Filter::new(&[], &["*.keep".into()]).unwrap();
        let p = build(
            &scan(vec![]),
            &[local_file("a.keep", 1, 0), local_file("b.txt", 1, 0)],
            Path::new("/dest"),
            &filter,
            &PlanOptions {
                delete: true,
                ..Default::default()
            },
            &Digests::default(),
        );
        assert_eq!(p.deletions, vec![PathBuf::from("/dest/b.txt")]);
    }
}
