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

/// One destination's current contents, as input to planning.
pub struct DestState<'a> {
    pub root: &'a Path,
    pub local: &'a [LocalEntry],
    pub digests: &'a Digests,
}

#[derive(Debug, Clone)]
pub struct TransferItem {
    pub rel: String,
    pub remote: String,
    pub size: u64,
    pub mtime: i64,
    /// Which destinations need this file. A file already correct everywhere is
    /// not in the plan at all; one that only a single destination is missing is
    /// read from the device once and written only there.
    pub targets: Vec<usize>,
}

/// Why a file is being transferred, for `-v` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Missing,
    SizeDiffers,
    TimeDiffers,
    ContentDiffers,
}

/// What one destination is due to receive and remove.
#[derive(Debug, Clone, Default)]
pub struct DestPlan {
    pub root: PathBuf,
    pub files: usize,
    pub bytes: u64,
    pub unchanged: usize,
    /// Paths to remove, deepest first so directories empty out.
    pub deletions: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Relative directories that must exist in every destination.
    pub dirs: Vec<String>,
    /// Files to read from the device, largest first.
    pub transfers: Vec<TransferItem>,
    pub reasons: HashMap<String, Reason>,
    pub dests: Vec<DestPlan>,
    pub filtered: usize,
    /// Symlinks on the device, which v0 does not reproduce.
    pub symlinks_skipped: usize,
    /// Bytes to read from the device. Each file counts once however many
    /// destinations receive it, because the device link is the scarce resource.
    pub total_bytes: u64,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.transfers.is_empty()
            && self.dirs.is_empty()
            && self.dests.iter().all(|d| d.deletions.is_empty())
    }

    pub fn deletions_total(&self) -> usize {
        self.dests.iter().map(|d| d.deletions.len()).sum()
    }

    /// Files already correct at every destination.
    pub fn unchanged_everywhere(&self) -> usize {
        self.dests.iter().map(|d| d.unchanged).min().unwrap_or(0)
    }
}

/// Compare the device tree against every destination and decide the work.
///
/// Each destination is diffed separately, and deliberately so. Diffing only
/// against one and letting the others take whatever it needs would be a little
/// simpler, but any destination that fell behind — a run interrupted partway, a
/// write that failed, a file removed by hand — would stay behind for good, with
/// nothing to notice it. Comparing each one means every run repairs whatever
/// has drifted, and the extra cost is close to nothing because the scans of
/// separate devices happen concurrently.
///
/// The default comparison is size plus mtime, which is rsync's quick check. The
/// rolling-checksum delta algorithm is deliberately absent: Android backup
/// corpora are dominated by immutable media, so it would force a full
/// device-side read of every candidate to save transfers that do not happen.
pub fn build(
    remote: &RemoteScan,
    dests: &[DestState<'_>],
    filter: &Filter,
    opts: &PlanOptions,
) -> Plan {
    let indexes: Vec<HashMap<&str, &LocalEntry>> = dests
        .iter()
        .map(|d| d.local.iter().map(|e| (e.rel.as_str(), e)).collect())
        .collect();

    let mut plan = Plan {
        dests: dests
            .iter()
            .map(|d| DestPlan {
                root: d.root.to_path_buf(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
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

                let mut targets = Vec::new();
                for (i, dest) in dests.iter().enumerate() {
                    let current = indexes[i].get(entry.rel.as_str()).copied();
                    match classify(entry, current, opts, dest.digests) {
                        Some(reason) => {
                            plan.reasons.entry(entry.rel.clone()).or_insert(reason);
                            plan.dests[i].files += 1;
                            plan.dests[i].bytes += entry.size;
                            targets.push(i);
                        }
                        None => plan.dests[i].unchanged += 1,
                    }
                }

                if !targets.is_empty() {
                    plan.total_bytes += entry.size;
                    plan.transfers.push(TransferItem {
                        rel: entry.rel.clone(),
                        remote: entry.remote.clone(),
                        size: entry.size,
                        mtime: entry.mtime,
                        targets,
                    });
                }
            }
        }
    }

    // Largest first, so the long tail of small files overlaps with the big
    // transfers instead of trailing after them as a latency-bound trickle.
    plan.transfers.sort_by_key(|t| std::cmp::Reverse(t.size));
    plan.dirs.sort();
    plan.dirs.dedup();

    if opts.delete {
        for (i, dest) in dests.iter().enumerate() {
            let mut extraneous: Vec<&LocalEntry> = dest
                .local
                .iter()
                .filter(|e| !wanted.contains(e.rel.as_str()))
                // Excluded paths are protected from deletion, as in rsync.
                .filter(|e| filter.accepts(&e.rel))
                .collect();
            // Deepest first so a directory is empty by the time it is removed.
            extraneous.sort_by_key(|e| std::cmp::Reverse(e.rel.len()));
            plan.dests[i].deletions = extraneous
                .into_iter()
                .map(|e| dest.root.join(&e.rel))
                .collect();
        }
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

    /// Plan against a single destination rooted at /dest.
    fn plan_of(remote: Vec<RemoteEntry>, local: Vec<LocalEntry>, opts: PlanOptions) -> Plan {
        let empty = Digests::default();
        build(
            &scan(remote),
            &[DestState {
                root: Path::new("/dest"),
                local: &local,
                digests: &empty,
            }],
            &Filter::default(),
            &opts,
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
        assert_eq!(p.transfers[0].targets, vec![0]);
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
        assert_eq!(p.dests[0].unchanged, 1);
    }

    #[test]
    fn tolerates_sub_second_mtime_drift() {
        let p = plan_of(
            vec![remote_file("a", 10, 100)],
            vec![local_file("a", 10, 101)],
            PlanOptions::default(),
        );
        assert_eq!(p.dests[0].unchanged, 1);

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
            p.dests[0].deletions,
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
        let local = vec![local_file("same", 999, 999)];

        // Sizes and times differ, but the content matches: no transfer.
        let p = build(
            &scan(vec![remote_file("same", 10, 100)]),
            &[DestState {
                root: Path::new("/dest"),
                local: &local,
                digests: &digests,
            }],
            &Filter::default(),
            &opts,
        );
        assert_eq!(p.dests[0].unchanged, 1);
        assert!(p.transfers.is_empty());

        // Size and time match, but the content does not: transfer.
        let local = vec![local_file("other", 10, 100)];
        let p = build(
            &scan(vec![remote_file("other", 10, 100)]),
            &[DestState {
                root: Path::new("/dest"),
                local: &local,
                digests: &digests,
            }],
            &Filter::default(),
            &opts,
        );
        assert_eq!(p.reasons["other"], Reason::ContentDiffers);
    }

    #[test]
    fn excluded_local_files_are_protected_from_deletion() {
        let filter = Filter::new(&[], &["*.keep".into()]).unwrap();
        let local = vec![local_file("a.keep", 1, 0), local_file("b.txt", 1, 0)];
        let empty = Digests::default();
        let p = build(
            &scan(vec![]),
            &[DestState {
                root: Path::new("/dest"),
                local: &local,
                digests: &empty,
            }],
            &filter,
            &PlanOptions {
                delete: true,
                ..Default::default()
            },
        );
        assert_eq!(p.dests[0].deletions, vec![PathBuf::from("/dest/b.txt")]);
    }

    // --- multiple destinations ---

    fn plan_two(remote: Vec<RemoteEntry>, a: Vec<LocalEntry>, b: Vec<LocalEntry>) -> Plan {
        let empty = Digests::default();
        build(
            &scan(remote),
            &[
                DestState {
                    root: Path::new("/m1"),
                    local: &a,
                    digests: &empty,
                },
                DestState {
                    root: Path::new("/m2"),
                    local: &b,
                    digests: &empty,
                },
            ],
            &Filter::default(),
            &PlanOptions::default(),
        )
    }

    #[test]
    fn a_file_missing_everywhere_is_read_once_and_written_to_all() {
        let p = plan_two(vec![remote_file("a", 10, 100)], vec![], vec![]);
        assert_eq!(p.transfers.len(), 1);
        assert_eq!(p.transfers[0].targets, vec![0, 1]);
        // The device link is the scarce resource, so the file counts once.
        assert_eq!(p.total_bytes, 10);
        assert_eq!(p.dests[0].bytes, 10);
        assert_eq!(p.dests[1].bytes, 10);
    }

    #[test]
    fn a_file_only_one_destination_lacks_is_written_only_there() {
        let p = plan_two(
            vec![remote_file("a", 10, 100)],
            vec![local_file("a", 10, 100)], // m1 already has it
            vec![],                         // m2 does not
        );
        assert_eq!(p.transfers.len(), 1);
        assert_eq!(p.transfers[0].targets, vec![1]);
        assert_eq!(p.dests[0].unchanged, 1);
        assert_eq!(p.dests[0].files, 0);
        assert_eq!(p.dests[1].files, 1);
    }

    #[test]
    fn a_destination_that_fell_behind_is_repaired_without_touching_the_others() {
        // The case that a single-reference diff would miss for good: an
        // interrupted run left m2 short of one file.
        let p = plan_two(
            vec![remote_file("a", 10, 100), remote_file("b", 20, 100)],
            vec![local_file("a", 10, 100), local_file("b", 20, 100)],
            vec![local_file("a", 10, 100)],
        );
        assert_eq!(p.transfers.len(), 1);
        assert_eq!(p.transfers[0].rel, "b");
        assert_eq!(p.transfers[0].targets, vec![1]);
        assert_eq!(p.unchanged_everywhere(), 1);
    }

    #[test]
    fn nothing_to_do_when_every_destination_is_current() {
        let p = plan_two(
            vec![remote_file("a", 10, 100)],
            vec![local_file("a", 10, 100)],
            vec![local_file("a", 10, 100)],
        );
        assert!(p.is_empty());
        assert_eq!(p.total_bytes, 0);
    }

    #[test]
    fn deletions_are_tracked_per_destination() {
        let empty = Digests::default();
        let a = vec![local_file("gone-from-m1", 1, 0)];
        let b = vec![];
        let p = build(
            &scan(vec![]),
            &[
                DestState {
                    root: Path::new("/m1"),
                    local: &a,
                    digests: &empty,
                },
                DestState {
                    root: Path::new("/m2"),
                    local: &b,
                    digests: &empty,
                },
            ],
            &Filter::default(),
            &PlanOptions {
                delete: true,
                ..Default::default()
            },
        );
        assert_eq!(p.dests[0].deletions.len(), 1);
        assert!(p.dests[1].deletions.is_empty());
        assert_eq!(p.deletions_total(), 1);
    }
}
