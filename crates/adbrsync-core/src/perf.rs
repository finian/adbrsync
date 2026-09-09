//! Machine-readable performance report for one run.
//!
//! The point of this file is to make later optimisation decisions checkable
//! against data instead of intuition, so it records what a run actually did
//! rather than a verdict about it. The two most useful parts are the per-size
//! histogram — which shows where time goes as a function of file size — and the
//! raw per-file samples, which allow any other analysis after the fact.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::error::Result;
use crate::plan::Plan;
use crate::scan::RemoteScan;
use crate::transfer::{Sample, TransferReport};

/// Bumped whenever the shape below changes incompatibly.
pub const SCHEMA: &str = "adbrsync.perf.v1";

#[derive(Debug, Default, Serialize)]
pub struct DeviceFacts {
    pub serial: String,
    pub model: Option<String>,
    pub android_release: Option<String>,
    pub sdk: Option<String>,
    pub abi: Option<String>,
    pub adb_server_version: Option<u32>,
    /// Intersection of host and device features, as negotiated.
    pub features: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct OptionFacts {
    pub streams: usize,
    pub recursive: bool,
    pub checksum: bool,
    pub delete: bool,
    pub preserve_mtime: bool,
    pub mtime_tolerance: i64,
    pub excludes: usize,
    pub includes: usize,
}

/// Wall-clock cost of each phase, so a slow run can be attributed.
#[derive(Debug, Default, Serialize)]
pub struct PhaseMillis {
    pub connect: u128,
    pub scan_remote: u128,
    pub scan_local: u128,
    pub checksum: u128,
    pub plan: u128,
    pub create_dirs: u128,
    pub transfer: u128,
    pub delete: u128,
    pub total: u128,
}

#[derive(Debug, Default, Serialize)]
pub struct SourceFacts {
    pub root: String,
    pub entries: usize,
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    /// Paths the device refused to read; a non-empty list means the copy is
    /// incomplete by design, not by accident.
    pub denied: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct PlanFacts {
    pub transfers: usize,
    pub bytes: u64,
    pub unchanged: usize,
    pub filtered: usize,
    pub deletions: usize,
    pub symlinks_skipped: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct TransferFacts {
    pub files: u64,
    pub bytes: u64,
    pub elapsed_ms: u128,
    pub streams: usize,
    pub stream_time_ms: u128,
    /// Share of available stream-seconds actually spent transferring. Below 1
    /// means streams idled, which is an argument for raising `--streams`.
    pub stream_utilization: f64,
    pub aggregate_bytes_per_sec: f64,
    pub per_stream_bytes_per_sec: f64,
    /// Null when the run held too few small files to measure it.
    pub per_file_fixed_ms: Option<f64>,
    pub fixed_cost_share: Option<f64>,
    pub failures: usize,
}

/// One size class, with how much time went into it.
#[derive(Debug, Serialize)]
pub struct SizeBucket {
    /// Upper bound of the class, exclusive. Null for the unbounded top class.
    pub upper_bytes: Option<u64>,
    pub files: usize,
    pub bytes: u64,
    pub total_ms: u128,
    pub mean_ms: f64,
    pub median_ms: f64,
    /// Bytes per second within this class, per stream.
    pub bytes_per_sec: f64,
}

/// Completed bytes and files per second of wall clock.
#[derive(Debug, Default, Serialize)]
pub struct Timeline {
    pub bucket_seconds: u64,
    pub bytes: Vec<u64>,
    pub files: Vec<u64>,
}

#[derive(Debug, Serialize)]
pub struct ErrorFact {
    pub path: String,
    pub message: String,
}

/// Raw observations, in a compact columnar form.
#[derive(Debug, Serialize)]
pub struct Samples {
    pub columns: [&'static str; 3],
    pub rows: Vec<[u64; 3]>,
}

#[derive(Debug, Serialize)]
pub struct PerfReport {
    pub schema: &'static str,
    pub tool_version: &'static str,
    pub started_at_unix: u64,
    pub source_path: String,
    pub dest_path: String,
    pub device: DeviceFacts,
    pub options: OptionFacts,
    pub phases_ms: PhaseMillis,
    pub source: SourceFacts,
    pub plan: PlanFacts,
    pub transfer: TransferFacts,
    pub size_buckets: Vec<SizeBucket>,
    pub timeline: Timeline,
    pub errors: Vec<ErrorFact>,
    pub samples: Samples,
}

/// Size classes, chosen to straddle the point where per-file cost stops
/// mattering and bytes start to.
const BUCKET_BOUNDS: [u64; 6] = [4 << 10, 64 << 10, 1 << 20, 16 << 20, 128 << 20, 1 << 30];

impl PerfReport {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        source_path: &str,
        dest: &Path,
        device: DeviceFacts,
        options: OptionFacts,
        phases_ms: PhaseMillis,
        scan: &RemoteScan,
        plan: &Plan,
        report: &TransferReport,
    ) -> Self {
        let source = SourceFacts {
            root: scan.root.clone(),
            entries: scan.entries.len(),
            files: count_kind(scan, crate::EntryKind::File),
            dirs: count_kind(scan, crate::EntryKind::Dir),
            symlinks: count_kind(scan, crate::EntryKind::Symlink),
            denied: scan.denied.clone(),
        };

        let transfer = TransferFacts {
            files: report.files,
            bytes: report.bytes,
            elapsed_ms: report.elapsed.as_millis(),
            streams: report.streams,
            stream_time_ms: report.stream_time.as_millis(),
            stream_utilization: report.stream_utilization(),
            aggregate_bytes_per_sec: report.throughput(),
            per_stream_bytes_per_sec: report.per_stream_rate,
            per_file_fixed_ms: report.per_file_fixed.map(|d| d.as_secs_f64() * 1000.0),
            fixed_cost_share: report.overhead_fraction(),
            failures: report.errors.len(),
        };

        Self {
            schema: SCHEMA,
            tool_version: env!("CARGO_PKG_VERSION"),
            started_at_unix: unix_now(),
            source_path: source_path.to_string(),
            dest_path: dest.display().to_string(),
            device,
            options,
            phases_ms,
            source,
            plan: PlanFacts {
                transfers: plan.transfers.len(),
                bytes: plan.total_bytes,
                unchanged: plan.unchanged,
                filtered: plan.filtered,
                deletions: plan.deletions.len(),
                symlinks_skipped: plan.symlinks_skipped,
            },
            transfer,
            size_buckets: bucketize(&report.samples),
            timeline: timeline(&report.samples, report.elapsed),
            errors: report
                .errors
                .iter()
                .map(|e| ErrorFact {
                    path: e.rel.clone(),
                    message: e.message.clone(),
                })
                .collect(),
            samples: Samples {
                columns: ["size_bytes", "duration_ms", "start_ms"],
                rows: report
                    .samples
                    .iter()
                    .map(|s| {
                        [
                            s.size,
                            s.duration.as_millis() as u64,
                            s.start.as_millis() as u64,
                        ]
                    })
                    .collect(),
            },
        }
    }

    pub fn write_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        serde_json::to_writer_pretty(std::io::BufWriter::new(file), self)
            .map_err(|e| crate::Error::Io(std::io::Error::other(e)))?;
        Ok(())
    }
}

fn count_kind(scan: &RemoteScan, kind: crate::EntryKind) -> usize {
    scan.entries.iter().filter(|e| e.kind == kind).count()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Group samples into size classes and summarise each.
fn bucketize(samples: &[Sample]) -> Vec<SizeBucket> {
    let mut grouped: BTreeMap<usize, Vec<&Sample>> = BTreeMap::new();
    for sample in samples {
        let index = BUCKET_BOUNDS
            .iter()
            .position(|bound| sample.size < *bound)
            .unwrap_or(BUCKET_BOUNDS.len());
        grouped.entry(index).or_default().push(sample);
    }

    grouped
        .into_iter()
        .map(|(index, mut items)| {
            let files = items.len();
            let bytes: u64 = items.iter().map(|s| s.size).sum();
            let total: Duration = items.iter().map(|s| s.duration).sum();
            items.sort_by_key(|s| s.duration);
            let median = items[files / 2].duration;
            SizeBucket {
                upper_bytes: BUCKET_BOUNDS.get(index).copied(),
                files,
                bytes,
                total_ms: total.as_millis(),
                mean_ms: total.as_secs_f64() * 1000.0 / files as f64,
                median_ms: median.as_secs_f64() * 1000.0,
                bytes_per_sec: if total.is_zero() {
                    0.0
                } else {
                    bytes as f64 / total.as_secs_f64()
                },
            }
        })
        .collect()
}

/// Completions bucketed by the second in which each file finished.
fn timeline(samples: &[Sample], elapsed: Duration) -> Timeline {
    let seconds = elapsed.as_secs().max(1) as usize + 1;
    let mut bytes = vec![0u64; seconds];
    let mut files = vec![0u64; seconds];
    for sample in samples {
        let finished = (sample.start + sample.duration).as_secs() as usize;
        let slot = finished.min(seconds - 1);
        bytes[slot] += sample.size;
        files[slot] += 1;
    }
    Timeline {
        bucket_seconds: 1,
        bytes,
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(size: u64, ms: u64, start_ms: u64) -> Sample {
        Sample {
            size,
            duration: Duration::from_millis(ms),
            start: Duration::from_millis(start_ms),
        }
    }

    #[test]
    fn buckets_split_on_size_boundaries() {
        let samples = vec![
            sample(1000, 10, 0),      // < 4 KiB
            sample(4096, 20, 0),      // 4 KiB is the next class up
            sample(100 << 20, 30, 0), // 128 MiB class
        ];
        let buckets = bucketize(&samples);
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].upper_bytes, Some(4 << 10));
        assert_eq!(buckets[0].files, 1);
        assert_eq!(buckets[1].upper_bytes, Some(64 << 10));
        assert_eq!(buckets[2].upper_bytes, Some(128 << 20));
    }

    #[test]
    fn the_largest_class_is_unbounded() {
        let buckets = bucketize(&[sample(4 << 30, 10, 0)]);
        assert_eq!(buckets[0].upper_bytes, None);
    }

    #[test]
    fn bucket_summary_uses_that_bucket_only() {
        let samples = vec![sample(1000, 10, 0), sample(2000, 30, 0)];
        let buckets = bucketize(&samples);
        assert_eq!(buckets[0].files, 2);
        assert_eq!(buckets[0].bytes, 3000);
        assert_eq!(buckets[0].total_ms, 40);
        assert!((buckets[0].mean_ms - 20.0).abs() < 1e-9);
    }

    #[test]
    fn timeline_counts_a_file_in_the_second_it_finished() {
        let samples = vec![
            sample(100, 500, 0),    // finishes at 0.5 s
            sample(200, 800, 1500), // finishes at 2.3 s
        ];
        let t = timeline(&samples, Duration::from_secs(3));
        assert_eq!(t.bytes[0], 100);
        assert_eq!(t.bytes[2], 200);
        assert_eq!(t.files[0], 1);
        assert_eq!(t.files[2], 1);
    }

    #[test]
    fn timeline_never_indexes_past_its_end() {
        let samples = vec![sample(1, 5_000, 9_000)];
        let t = timeline(&samples, Duration::from_secs(2));
        assert_eq!(t.bytes.iter().sum::<u64>(), 1);
    }
}
