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
use crate::transfer::{ProgressSample, Sample, TransferReport};

/// Bumped whenever the shape below changes incompatibly.
pub const SCHEMA: &str = "adbrsync.perf.v2";

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
    /// Files to read from the device. One read may serve several destinations.
    pub transfers: usize,
    /// Bytes to read from the device, counting each file once.
    pub bytes: u64,
    pub filtered: usize,
    pub symlinks_skipped: usize,
    pub unchanged_everywhere: usize,
    pub destinations: Vec<DestFacts>,
}

/// What one destination was due to receive, and what it actually got.
#[derive(Debug, Default, Serialize)]
pub struct DestFacts {
    pub root: String,
    pub planned_files: usize,
    pub planned_bytes: u64,
    pub unchanged: usize,
    pub deletions: usize,
    pub written_files: u64,
    pub written_bytes: u64,
    pub failures: usize,
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
    /// How many files, and up to what size, that estimate came from.
    pub per_file_fixed_from_files: Option<usize>,
    pub per_file_fixed_up_to_bytes: Option<u64>,
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

/// Throughput over the life of the transfer.
///
/// Built by differencing periodic readings of the live counters. An earlier
/// version bucketed files by the instant each one *finished*, which on a run of
/// large files left most buckets empty and put a whole file's bytes into the one
/// second it completed in — reporting spikes many times the real rate. Bytes are
/// now counted as they are written, so these readings are the actual curve.
#[derive(Debug, Default, Serialize)]
pub struct Timeline {
    pub sample_interval_ms: u64,
    /// Milliseconds from the start of the transfer for each reading.
    pub at_ms: Vec<u64>,
    /// Bytes written since the previous reading.
    pub bytes: Vec<u64>,
    /// Files completed since the previous reading.
    pub files: Vec<u64>,
    /// Throughput across each interval.
    pub bytes_per_sec: Vec<f64>,
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
    pub dest_paths: Vec<String>,
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
        dests: &[std::path::PathBuf],
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
            per_file_fixed_from_files: report.fixed_basis.map(|(n, _)| n),
            per_file_fixed_up_to_bytes: report.fixed_basis.map(|(_, s)| s),
            fixed_cost_share: report.overhead_fraction(),
            failures: report.errors.len(),
        };

        Self {
            schema: SCHEMA,
            tool_version: env!("CARGO_PKG_VERSION"),
            started_at_unix: unix_now(),
            source_path: source_path.to_string(),
            dest_paths: dests.iter().map(|d| d.display().to_string()).collect(),
            device,
            options,
            phases_ms,
            source,
            plan: PlanFacts {
                transfers: plan.transfers.len(),
                bytes: plan.total_bytes,
                filtered: plan.filtered,
                symlinks_skipped: plan.symlinks_skipped,
                unchanged_everywhere: plan.unchanged_everywhere(),
                destinations: plan
                    .dests
                    .iter()
                    .enumerate()
                    .map(|(i, d)| {
                        let got = report.dests.get(i);
                        DestFacts {
                            root: d.root.display().to_string(),
                            planned_files: d.files,
                            planned_bytes: d.bytes,
                            unchanged: d.unchanged,
                            deletions: d.deletions.len(),
                            written_files: got.map(|o| o.files).unwrap_or(0),
                            written_bytes: got.map(|o| o.bytes).unwrap_or(0),
                            failures: got.map(|o| o.failures).unwrap_or(0),
                        }
                    })
                    .collect(),
            },
            transfer,
            size_buckets: bucketize(&report.samples),
            timeline: timeline(&report.progress),
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

/// Difference the counter readings into per-interval throughput.
fn timeline(progress: &[ProgressSample]) -> Timeline {
    let mut t = Timeline {
        sample_interval_ms: 0,
        ..Default::default()
    };
    let (mut prev_at, mut prev_bytes, mut prev_files) = (Duration::ZERO, 0u64, 0u64);
    for sample in progress {
        let span = sample.at.saturating_sub(prev_at);
        if span.is_zero() {
            continue;
        }
        let bytes = sample.bytes_done.saturating_sub(prev_bytes);
        t.at_ms.push(sample.at.as_millis() as u64);
        t.bytes.push(bytes);
        t.files.push(sample.files_done.saturating_sub(prev_files));
        t.bytes_per_sec.push(bytes as f64 / span.as_secs_f64());
        prev_at = sample.at;
        prev_bytes = sample.bytes_done;
        prev_files = sample.files_done;
    }
    // Report the interval actually observed rather than the one asked for.
    t.sample_interval_ms = t
        .at_ms
        .windows(2)
        .map(|w| w[1] - w[0])
        .min()
        .unwrap_or_else(|| t.at_ms.first().copied().unwrap_or(0));
    t
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

    fn reading(at_ms: u64, bytes: u64, files: u64) -> ProgressSample {
        ProgressSample {
            at: Duration::from_millis(at_ms),
            bytes_done: bytes,
            files_done: files,
        }
    }

    #[test]
    fn timeline_differences_counter_readings() {
        let t = timeline(&[
            reading(250, 1_000, 0),
            reading(500, 3_000, 1),
            reading(750, 3_500, 1),
        ]);
        assert_eq!(t.at_ms, vec![250, 500, 750]);
        assert_eq!(t.bytes, vec![1_000, 2_000, 500]);
        assert_eq!(t.files, vec![0, 1, 0]);
        // 2,000 bytes across 250 ms is 8,000 B/s.
        assert!((t.bytes_per_sec[1] - 8_000.0).abs() < 1e-6);
        assert_eq!(t.sample_interval_ms, 250);
    }

    #[test]
    fn timeline_shows_a_stall_as_zero_not_as_a_spike() {
        // A long file in flight: bytes keep arriving, no file completes.
        let t = timeline(&[
            reading(250, 5_000, 0),
            reading(500, 5_000, 0),
            reading(750, 10_000, 1),
        ]);
        assert_eq!(t.bytes, vec![5_000, 0, 5_000]);
        assert_eq!(t.bytes_per_sec[1], 0.0);
    }

    #[test]
    fn timeline_of_an_empty_run_is_empty() {
        let t = timeline(&[]);
        assert!(t.at_ms.is_empty());
        assert_eq!(t.sample_interval_ms, 0);
    }

    #[test]
    fn timeline_ignores_readings_with_no_elapsed_time() {
        let t = timeline(&[reading(250, 100, 1), reading(250, 200, 2)]);
        assert_eq!(t.at_ms, vec![250]);
    }
}
