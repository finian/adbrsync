use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use adb_proto::{AdbClient, DeviceSelector, SyncSession};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::entry::safe_join;
use crate::error::{Error, Result};
use crate::plan::{Plan, TransferItem};

#[derive(Debug, Clone)]
pub struct TransferOptions {
    /// Number of concurrent sync streams. Each is its own connection to the
    /// adb server, because one sync stream cannot have two requests in flight.
    pub streams: usize,
    pub preserve_mtime: bool,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            // Measured optimum on the reference device over a wireless link;
            // throughput rises steeply to 16 and falls off slowly past it.
            streams: 16,
            preserve_mtime: true,
        }
    }
}

/// Live counters, readable while the transfer runs.
#[derive(Debug, Default)]
pub struct Stats {
    pub files_done: AtomicU64,
    /// Advanced as bytes are written, not when a file finishes.
    pub bytes_done: Arc<AtomicU64>,
    pub files_total: AtomicU64,
    pub bytes_total: AtomicU64,
}

/// A reading of the live counters, taken while the transfer runs.
///
/// Sampling the counters is the only way to see throughput over time: a record
/// of when each file *finished* says nothing about the seconds in between, and
/// with large files those are nearly all of them.
#[derive(Debug, Clone, Copy)]
pub struct ProgressSample {
    pub at: Duration,
    pub bytes_done: u64,
    pub files_done: u64,
}

/// One completed file transfer, kept so a run can be analysed afterwards.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub size: u64,
    pub duration: Duration,
    /// When this file began, measured from the start of the transfer.
    pub start: Duration,
}

#[derive(Debug, Clone)]
pub struct FileError {
    pub rel: String,
    pub message: String,
}

#[derive(Debug)]
pub struct TransferReport {
    pub files: u64,
    pub bytes: u64,
    pub errors: Vec<FileError>,
    pub elapsed: Duration,
    /// Summed per-file durations across all streams.
    pub stream_time: Duration,
    /// Fixed cost per file, independent of its size.
    ///
    /// `None` when the run contained too few small files to measure it: with
    /// only large files, time spent per file is dominated by bytes and the two
    /// cannot be told apart.
    pub per_file_fixed: Option<Duration>,
    /// Sustained per-stream byte rate implied by the same fit.
    pub per_stream_rate: f64,
    /// Streams the transfer actually ran on.
    pub streams: usize,
    /// Every completed transfer, for offline analysis.
    pub samples: Vec<Sample>,
    /// Counter readings taken during the run, in order.
    pub progress: Vec<ProgressSample>,
}

impl TransferReport {
    /// A report for a run that transferred nothing, such as a dry run.
    pub fn empty() -> Self {
        Self {
            files: 0,
            bytes: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
            stream_time: Duration::ZERO,
            per_file_fixed: None,
            per_stream_rate: 0.0,
            streams: 0,
            samples: Vec::new(),
            progress: Vec::new(),
        }
    }

    /// Wall time attributable to per-file fixed cost.
    ///
    /// Fixed cost is paid per file but spread across streams, so N files cost
    /// `N * fixed / streams` of wall time, not `N * fixed`.
    pub fn fixed_wall_cost(&self) -> Option<Duration> {
        if self.streams == 0 {
            return None;
        }
        self.per_file_fixed
            .map(|fixed| fixed.mul_f64(self.files as f64 / self.streams as f64))
    }

    /// Share of wall time spent on per-file cost rather than moving bytes.
    ///
    /// This informs whether an on-device agent would be worth building:
    /// batching many files into one stream only pays off when the fraction is
    /// large. It is measured against wall time, because fixed cost on one
    /// stream overlaps with bytes moving on the others — a saturated link
    /// hides per-file cost almost entirely.
    ///
    /// `None` when `per_file_fixed` could not be measured.
    pub fn overhead_fraction(&self) -> Option<f64> {
        if self.elapsed.is_zero() {
            return None;
        }
        self.fixed_wall_cost()
            .map(|cost| (cost.as_secs_f64() / self.elapsed.as_secs_f64()).min(1.0))
    }

    /// Fraction of the available stream-seconds actually spent transferring.
    ///
    /// Unlike the overhead estimate this is a direct measurement. A value well
    /// below 1 means streams sat idle, which is a reason to raise `--streams`.
    pub fn stream_utilization(&self) -> f64 {
        let available = self.elapsed.as_secs_f64() * self.streams as f64;
        if available <= 0.0 {
            return 0.0;
        }
        (self.stream_time.as_secs_f64() / available).min(1.0)
    }

    pub fn throughput(&self) -> f64 {
        if self.elapsed.is_zero() {
            return 0.0;
        }
        self.bytes as f64 / self.elapsed.as_secs_f64()
    }
}

/// Create every directory the plan needs, before any transfer starts.
pub async fn create_dirs(plan: &Plan, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).await?;
    for rel in &plan.dirs {
        let path = safe_join(dest, rel).ok_or_else(|| Error::UnsafePath(rel.clone()))?;
        fs::create_dir_all(&path).await?;
    }
    Ok(())
}

/// Run the transfers over `opts.streams` concurrent sync sessions.
pub async fn run(
    client: &AdbClient,
    selector: &DeviceSelector,
    plan: &Plan,
    dest: &Path,
    opts: &TransferOptions,
    stats: Arc<Stats>,
) -> Result<TransferReport> {
    stats
        .files_total
        .store(plan.transfers.len() as u64, Ordering::Relaxed);
    stats.bytes_total.store(plan.total_bytes, Ordering::Relaxed);

    if plan.transfers.is_empty() {
        return Ok(TransferReport::empty());
    }

    // Already sorted largest-first by the planner; workers pull as they free up.
    let queue: Arc<Mutex<VecDeque<TransferItem>>> =
        Arc::new(Mutex::new(plan.transfers.iter().cloned().collect()));

    let stream_count = opts.streams.clamp(1, plan.transfers.len());
    let started = Instant::now();

    // Sample the counters on a timer, so the report can show throughput over
    // time rather than only where files happened to finish.
    let progress_log: Arc<Mutex<Vec<ProgressSample>>> = Arc::new(Mutex::new(Vec::new()));
    let sampler = {
        let stats = Arc::clone(&stats);
        let log = Arc::clone(&progress_log);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PROGRESS_SAMPLE_INTERVAL);
            ticker.tick().await; // fires immediately; that is the zero reading
            loop {
                ticker.tick().await;
                log.lock().expect("progress log").push(ProgressSample {
                    at: started.elapsed(),
                    bytes_done: stats.bytes_done.load(Ordering::Relaxed),
                    files_done: stats.files_done.load(Ordering::Relaxed),
                });
            }
        })
    };

    let mut workers = Vec::with_capacity(stream_count);

    for _ in 0..stream_count {
        let client = client.clone();
        let selector = selector.clone();
        let queue = Arc::clone(&queue);
        let stats = Arc::clone(&stats);
        let dest = dest.to_path_buf();
        let preserve_mtime = opts.preserve_mtime;
        let origin = started;

        workers.push(tokio::spawn(async move {
            let mut session = match open_session(&client, &selector).await {
                Ok(s) => s,
                // One stream failing to start is survivable: its share of the
                // queue is simply picked up by the streams that did start.
                Err(e) => return WorkerResult::stream_failed(e),
            };
            let mut result = WorkerResult::default();
            loop {
                let Some(item) = queue.lock().expect("queue mutex").pop_front() else {
                    break;
                };
                let began = Instant::now();
                match fetch_one(&mut session, &item, &dest, preserve_mtime, &stats).await {
                    Ok(bytes) => {
                        result.samples.push(Sample {
                            size: bytes,
                            duration: began.elapsed(),
                            start: began.duration_since(origin),
                        });
                        result.files += 1;
                        result.bytes += bytes;
                        // Bytes are counted as they are written, not here.
                        stats.files_done.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        // One unreadable file must not abandon the rest.
                        let broke_stream = breaks_stream(&e);
                        result.errors.push(FileError {
                            rel: item.rel.clone(),
                            message: e.to_string(),
                        });
                        stats.files_done.fetch_add(1, Ordering::Relaxed);

                        // A transport-level failure leaves the session
                        // mid-response, so every later file on it would fail
                        // too. Replace it, or stop and let the other streams
                        // drain the queue.
                        if broke_stream {
                            match open_session(&client, &selector).await {
                                Ok(fresh) => session = fresh,
                                Err(e) => {
                                    result.errors.push(FileError {
                                        rel: "<stream>".to_string(),
                                        message: format!("stream lost and not recoverable: {e}"),
                                    });
                                    return result;
                                }
                            }
                        }
                    }
                }
            }
            let _ = session.quit().await;
            result
        }));
    }

    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut errors = Vec::new();
    let mut samples: Vec<Sample> = Vec::new();
    let mut streams_started = 0usize;
    let mut first_stream_error = None;

    for worker in workers {
        let result = worker.await.map_err(|e| {
            Error::Io(std::io::Error::other(format!(
                "transfer task panicked: {e}"
            )))
        })?;
        match result.stream_error {
            Some(e) => first_stream_error = first_stream_error.or(Some(e)),
            None => streams_started += 1,
        }
        files += result.files;
        bytes += result.bytes;
        errors.extend(result.errors);
        samples.extend(result.samples);
    }

    // Only a total failure to open any stream is fatal.
    if streams_started == 0 {
        return Err(first_stream_error.unwrap_or_else(|| {
            Error::Io(std::io::Error::other("no sync stream could be opened"))
        }));
    }
    if streams_started < stream_count {
        errors.push(FileError {
            rel: "<streams>".to_string(),
            message: format!(
                "{} of {stream_count} sync streams could not be opened;                  the transfer ran at reduced concurrency",
                stream_count - streams_started
            ),
        });
    }

    sampler.abort();
    let mut progress = progress_log.lock().expect("progress log").clone();
    progress.push(ProgressSample {
        at: started.elapsed(),
        bytes_done: stats.bytes_done.load(Ordering::Relaxed),
        files_done: stats.files_done.load(Ordering::Relaxed),
    });

    samples.sort_by_key(|s| s.start);
    let fit = measure_cost(&samples);
    Ok(TransferReport {
        files,
        bytes,
        errors,
        elapsed: started.elapsed(),
        stream_time: samples.iter().map(|s| s.duration).sum(),
        per_file_fixed: fit.fixed,
        per_stream_rate: fit.rate,
        streams: streams_started,
        samples,
        progress,
    })
}

#[derive(Default)]
struct WorkerResult {
    files: u64,
    bytes: u64,
    errors: Vec<FileError>,
    samples: Vec<Sample>,
    /// Set when this worker never got a sync stream at all.
    stream_error: Option<Error>,
}

impl WorkerResult {
    fn stream_failed(e: Error) -> Self {
        Self {
            stream_error: Some(e),
            ..Default::default()
        }
    }
}

/// Whether an error leaves the sync session unusable.
///
/// A `FAIL` response is part of the protocol and the session survives it; an
/// io or framing error means the stream is out of sync or gone.
fn breaks_stream(error: &Error) -> bool {
    match error {
        Error::Adb(adb_proto::Error::Sync { .. }) => false,
        Error::Adb(_) => true,
        Error::Io(_) => true,
        _ => false,
    }
}

/// Opening a stream can fail transiently while the adb server reclaims sockets
/// from a previous run, so give it a couple of chances before giving up.
async fn open_session(client: &AdbClient, selector: &DeviceSelector) -> Result<SyncSession> {
    const ATTEMPTS: u32 = 3;
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match SyncSession::open(client, selector).await {
            Ok(session) => return Ok(session),
            Err(e) => {
                last = Some(Error::Adb(e));
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
                }
            }
        }
    }
    Err(last.expect("at least one attempt"))
}

/// Pull one file to a temporary name and rename it into place, so an
/// interrupted run never leaves a truncated file that looks complete.
async fn fetch_one(
    session: &mut SyncSession,
    item: &TransferItem,
    dest: &Path,
    preserve_mtime: bool,
    stats: &Arc<Stats>,
) -> Result<u64> {
    let final_path =
        safe_join(dest, &item.rel).ok_or_else(|| Error::UnsafePath(item.rel.clone()))?;
    let parent = final_path
        .parent()
        .ok_or_else(|| Error::UnsafePath(item.rel.clone()))?;
    fs::create_dir_all(parent).await?;

    let temp_path = temp_path_for(&final_path);
    let file = fs::File::create(&temp_path).await?;
    let mut counted = CountingWriter::new(file, Arc::clone(&stats.bytes_done));
    let outcome = session.recv(&item.remote, &mut counted).await;
    let bytes = match outcome {
        Ok(bytes) => {
            counted.flush().await?;
            drop(counted);
            bytes
        }
        Err(e) => {
            // The partial file is discarded, so its bytes must leave the
            // counter too or progress would run past the total.
            stats
                .bytes_done
                .fetch_sub(counted.written(), Ordering::Relaxed);
            drop(counted);
            let _ = fs::remove_file(&temp_path).await;
            return Err(e.into());
        }
    };

    fs::rename(&temp_path, &final_path).await?;
    if preserve_mtime {
        let path = final_path.clone();
        let mtime = item.mtime;
        tokio::task::spawn_blocking(move || {
            filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(mtime, 0))
        })
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))??;
    }
    Ok(bytes)
}

/// How often the live counters are sampled for the timeline.
const PROGRESS_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// Writer that adds each chunk to a counter as it lands.
///
/// Counting on completion instead would make both the progress display and the
/// recorded timeline move in whole-file steps, which for multi-megabyte files
/// means minutes of apparent stillness.
struct CountingWriter<W> {
    inner: W,
    counter: Arc<AtomicU64>,
    written: u64,
}

impl<W> CountingWriter<W> {
    fn new(inner: W, counter: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            counter,
            written: 0,
        }
    }

    fn written(&self) -> u64 {
        self.written
    }
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for CountingWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let written = std::task::ready!(Pin::new(&mut self.inner).poll_write(cx, buf))?;
        self.counter.fetch_add(written as u64, Ordering::Relaxed);
        self.written += written as u64;
        Poll::Ready(Ok(written))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn temp_path_for(final_path: &Path) -> PathBuf {
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    final_path.with_file_name(format!(".{name}.adbrsync-tmp"))
}

/// Measure per-file fixed cost and per-stream byte rate.
///
/// Both come from direct observation rather than a model. The rate is simply
/// bytes over stream-seconds. The fixed cost is the median duration of the
/// files small enough that their bytes cost almost nothing, with that little
/// remaining byte time subtracted.
///
/// An earlier version fitted `duration = fixed + size / rate` by least squares
/// over all files. That fails on a run of uniformly large files: with almost no
/// spread in size, the slope and intercept are not separately identifiable and
/// the fit happily reports seconds of "fixed cost" per file. Refusing to answer
/// is better than answering confidently from data that cannot support it.
fn measure_cost(samples: &[Sample]) -> CostModel {
    /// Files at or below this size spend almost all their time on fixed cost.
    const SMALL: u64 = 4096;
    /// Below this many small files the median is too noisy to be worth quoting.
    const MIN_SMALL_SAMPLES: usize = 20;

    let stream_time: f64 = samples.iter().map(|s| s.duration.as_secs_f64()).sum();
    let bytes: u64 = samples.iter().map(|s| s.size).sum();
    let rate = if stream_time > 0.0 {
        bytes as f64 / stream_time
    } else {
        0.0
    };

    let mut small: Vec<Duration> = samples
        .iter()
        .filter(|s| s.size <= SMALL)
        .map(|s| s.duration)
        .collect();
    if small.len() < MIN_SMALL_SAMPLES {
        return CostModel { fixed: None, rate };
    }
    small.sort_unstable();
    let median = small[small.len() / 2];

    let mean_small_bytes = samples
        .iter()
        .filter(|s| s.size <= SMALL)
        .map(|s| s.size)
        .sum::<u64>() as f64
        / small.len() as f64;
    let byte_time = if rate > 0.0 {
        Duration::from_secs_f64(mean_small_bytes / rate)
    } else {
        Duration::ZERO
    };

    CostModel {
        fixed: Some(median.saturating_sub(byte_time)),
        rate,
    }
}

struct CostModel {
    fixed: Option<Duration>,
    /// Bytes per second per stream.
    rate: f64,
}

/// Remove the paths the plan marked extraneous. Deepest first, so directories
/// are empty by the time they are reached.
pub async fn apply_deletions(paths: &[PathBuf]) -> Vec<FileError> {
    let mut errors = Vec::new();
    for path in paths {
        let outcome = match fs::metadata(path).await {
            Ok(meta) if meta.is_dir() => fs::remove_dir(path).await,
            Ok(_) => fs::remove_file(path).await,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => Err(e),
        };
        if let Err(e) = outcome {
            errors.push(FileError {
                rel: path.display().to_string(),
                message: e.to_string(),
            });
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sync_fail_response_keeps_the_session() {
        let e = Error::Adb(adb_proto::Error::Sync {
            path: "/sdcard/x".into(),
            reason: "open failed: Permission denied".into(),
        });
        assert!(!breaks_stream(&e));
    }

    #[test]
    fn a_transport_failure_invalidates_the_session() {
        let e = Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "early eof",
        ));
        assert!(breaks_stream(&e));

        let e = Error::Adb(adb_proto::Error::Protocol("bad header".into()));
        assert!(breaks_stream(&e));
    }

    #[test]
    fn temp_name_sits_beside_the_target() {
        let p = temp_path_for(Path::new("/dest/a/b.jpg"));
        assert_eq!(p, PathBuf::from("/dest/a/.b.jpg.adbrsync-tmp"));
        assert_eq!(p.parent(), Path::new("/dest/a/b.jpg").parent());
    }

    fn sample(size: u64, duration: Duration) -> Sample {
        Sample {
            size,
            duration,
            start: Duration::ZERO,
        }
    }

    fn mixed_samples() -> Vec<Sample> {
        // 30 tiny files at 100 ms each, plus large files at 1 MiB/s.
        let mut v: Vec<Sample> = (0..30)
            .map(|_| sample(1024, Duration::from_millis(100)))
            .collect();
        v.extend((1..=5).map(|mib: u64| sample(mib << 20, Duration::from_secs(mib))));
        v
    }

    #[test]
    fn measures_fixed_cost_from_the_small_files() {
        let cost = measure_cost(&mixed_samples());
        let fixed = cost.fixed.expect("enough small files");
        // 100 ms less the negligible byte time of a 1 KiB file.
        assert!((fixed.as_secs_f64() - 0.1).abs() < 0.005, "{fixed:?}");
    }

    #[test]
    fn rate_is_bytes_over_stream_seconds() {
        let cost = measure_cost(&mixed_samples());
        let bytes: u64 = mixed_samples().iter().map(|s| s.size).sum();
        let secs: f64 = mixed_samples()
            .iter()
            .map(|s| s.duration.as_secs_f64())
            .sum();
        assert!((cost.rate - bytes as f64 / secs).abs() < 1.0);
    }

    #[test]
    fn refuses_to_guess_when_every_file_is_large() {
        // Uniformly large files: fixed cost is not identifiable from them.
        let samples: Vec<Sample> = (1..=40)
            .map(|i: u64| sample(18 << 20, Duration::from_secs_f64(20.0 + i as f64 * 0.1)))
            .collect();
        let cost = measure_cost(&samples);
        assert!(cost.fixed.is_none());
        assert!(cost.rate > 0.0);
    }

    #[test]
    fn overhead_is_measured_against_wall_time_and_divided_by_streams() {
        let report = TransferReport {
            files: 80,
            bytes: 0,
            errors: vec![],
            elapsed: Duration::from_secs(10),
            stream_time: Duration::from_secs(80),
            per_file_fixed: Some(Duration::from_millis(100)),
            per_stream_rate: 1.0,
            streams: 8,
            samples: Vec::new(),
            progress: Vec::new(),
        };
        // 80 files * 100 ms / 8 streams = 1 s of the 10 s wall clock.
        assert_eq!(report.fixed_wall_cost(), Some(Duration::from_secs(1)));
        assert!((report.overhead_fraction().unwrap() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn unmeasurable_fixed_cost_yields_no_fraction() {
        let report = TransferReport {
            files: 10,
            bytes: 0,
            errors: vec![],
            elapsed: Duration::from_secs(10),
            stream_time: Duration::from_secs(80),
            per_file_fixed: None,
            per_stream_rate: 1.0,
            streams: 8,
            samples: Vec::new(),
            progress: Vec::new(),
        };
        assert!(report.fixed_wall_cost().is_none());
        assert!(report.overhead_fraction().is_none());
    }

    #[test]
    fn empty_samples_do_not_divide_by_zero() {
        let cost = measure_cost(&[]);
        assert!(cost.fixed.is_none());
        assert_eq!(cost.rate, 0.0);
    }
}
