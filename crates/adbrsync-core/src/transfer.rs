use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Bytes read from the device, advanced as they arrive rather than when a
    /// file finishes. A file bound for several destinations counts once here,
    /// because the device link is what the progress display is about.
    pub bytes_done: Arc<AtomicU64>,
    pub files_total: AtomicU64,
    pub bytes_total: AtomicU64,
    pub dests: Vec<DestStats>,
}

impl Stats {
    pub fn new(destinations: usize) -> Self {
        Self {
            dests: (0..destinations).map(|_| DestStats::default()).collect(),
            ..Default::default()
        }
    }
}

/// What one destination has actually received so far.
#[derive(Debug, Default)]
pub struct DestStats {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub failures: AtomicU64,
    /// Set once a destination has been written off. Later files skip it
    /// silently rather than reporting the same broken drive thousands of times.
    pub disabled: AtomicBool,
}

/// Failures on one destination before it is written off for the rest of the run.
const DEST_FAILURE_LIMIT: u64 = 10;

impl DestStats {
    fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }

    /// Record a failure; returns true if this is the one that gives up.
    fn note_failure(&self) -> bool {
        let count = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        count == DEST_FAILURE_LIMIT && !self.disabled.swap(true, Ordering::Relaxed)
    }

    pub fn disable(&self) {
        self.disabled.store(true, Ordering::Relaxed);
    }
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
    /// How many files, and up to what size, the estimate above was taken from.
    pub fixed_basis: Option<(usize, u64)>,
    /// Sustained per-stream byte rate implied by the same fit.
    pub per_stream_rate: f64,
    /// Streams the transfer actually ran on.
    pub streams: usize,
    /// Every completed transfer, for offline analysis.
    pub samples: Vec<Sample>,
    /// Counter readings taken during the run, in order.
    pub progress: Vec<ProgressSample>,
    /// What each destination ended up with.
    pub dests: Vec<DestOutcome>,
}

/// One destination's share of a finished run.
#[derive(Debug, Clone, Default)]
pub struct DestOutcome {
    pub root: PathBuf,
    pub files: u64,
    pub bytes: u64,
    pub failures: usize,
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
            fixed_basis: None,
            per_stream_rate: 0.0,
            streams: 0,
            samples: Vec::new(),
            progress: Vec::new(),
            dests: Vec::new(),
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

/// Create every directory the plan needs, in every destination, before any
/// transfer starts.
///
/// A destination that cannot be prepared — read-only, full, unplugged between
/// the check and now — is reported and left out, not allowed to abort the run.
/// Losing one drive should not cost you the copy on the other. Only when every
/// destination fails is there nothing left to do.
pub async fn create_dirs(plan: &Plan, dests: &[PathBuf]) -> Result<Vec<(usize, String)>> {
    let mut failed = Vec::new();
    for (i, dest) in dests.iter().enumerate() {
        if let Err(e) = prepare_dest(plan, dest).await {
            failed.push((i, e.to_string()));
        }
    }
    if failed.len() == dests.len() {
        let reason = failed
            .first()
            .map(|(_, e)| e.clone())
            .unwrap_or_else(|| "no destinations".to_string());
        return Err(Error::Io(std::io::Error::other(format!(
            "no destination could be prepared: {reason}"
        ))));
    }
    Ok(failed)
}

async fn prepare_dest(plan: &Plan, dest: &Path) -> Result<()> {
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
    dests: &[PathBuf],
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
        let dests: Vec<PathBuf> = dests.to_vec();
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
                if stats.dests.iter().all(DestStats::is_disabled) {
                    break;
                }
                let Some(item) = queue.lock().expect("queue mutex").pop_front() else {
                    break;
                };
                let began = Instant::now();
                match fetch_one(&mut session, &item, &dests, preserve_mtime, &stats).await {
                    Ok(fetched) => {
                        let bytes = fetched.bytes;
                        for (target, message) in fetched.failed {
                            result.errors.push(FileError {
                                rel: format!("{} -> {}", item.rel, dests[target].display()),
                                message,
                            });
                        }
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
        fixed_basis: fit.basis,
        per_stream_rate: fit.rate,
        streams: streams_started,
        samples,
        progress,
        dests: dests
            .iter()
            .enumerate()
            .map(|(i, root)| DestOutcome {
                root: root.clone(),
                files: stats.dests[i].files.load(Ordering::Relaxed),
                bytes: stats.dests[i].bytes.load(Ordering::Relaxed),
                failures: stats.dests[i].failures.load(Ordering::Relaxed) as usize,
            })
            .collect(),
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

/// What one file's transfer produced.
struct FetchOutcome {
    /// Bytes read from the device, counted once however many destinations
    /// received them.
    bytes: u64,
    /// Destinations that could not be written, by index into the destination
    /// list, with the reason.
    failed: Vec<(usize, String)>,
}

/// Pull one file and write it to every destination that needs it.
///
/// Each destination is written to a temporary name and renamed into place, so
/// an interrupted run never leaves a truncated file that looks complete. A
/// destination that fails does not take the others down with it — a full disk
/// on one drive should not cost you the copy on the other.
async fn fetch_one(
    session: &mut SyncSession,
    item: &TransferItem,
    dests: &[PathBuf],
    preserve_mtime: bool,
    stats: &Arc<Stats>,
) -> Result<FetchOutcome> {
    let mut targets: Vec<(usize, PathBuf, PathBuf)> = Vec::new();
    let mut files: Vec<Option<fs::File>> = Vec::new();
    let mut failed: Vec<(usize, String)> = Vec::new();

    for &i in &item.targets {
        if stats.dests[i].is_disabled() {
            continue;
        }
        let Some(final_path) = safe_join(&dests[i], &item.rel) else {
            failed.push((i, "refusing unsafe destination path".to_string()));
            continue;
        };
        let Some(parent) = final_path.parent().map(Path::to_path_buf) else {
            failed.push((i, "destination path has no parent".to_string()));
            continue;
        };
        if let Err(e) = fs::create_dir_all(&parent).await {
            failed.push((i, e.to_string()));
            continue;
        }
        let temp_path = temp_path_for(&final_path);
        match fs::File::create(&temp_path).await {
            Ok(file) => {
                files.push(Some(file));
                targets.push((i, final_path, temp_path));
            }
            Err(e) => failed.push((i, e.to_string())),
        }
    }

    for (i, _) in &failed {
        note_failure(stats, *i, &mut Vec::new());
    }
    if files.is_empty() {
        if item.targets.iter().all(|i| stats.dests[*i].is_disabled()) {
            // Everything this file was bound for has already been written off.
            return Ok(FetchOutcome {
                bytes: 0,
                failed: Vec::new(),
            });
        }
        return Err(Error::Io(std::io::Error::other(
            "no destination could be opened for writing",
        )));
    }

    let mut sink = Fanout::new(files, Arc::clone(&stats.bytes_done));
    let received = session.recv(&item.remote, &mut sink).await;
    if let Err(e) = received {
        // The partial files are discarded, so their bytes must leave the
        // counter too or progress would run past the total.
        stats
            .bytes_done
            .fetch_sub(sink.written(), Ordering::Relaxed);
        drop(sink);
        for (_, _, temp_path) in &targets {
            let _ = fs::remove_file(temp_path).await;
        }
        return Err(e.into());
    }
    let bytes = received.expect("checked above");

    // A flush error only surfaces once every destination has failed; the
    // per-destination reasons are collected by the sink either way.
    let _ = sink.flush().await;
    let (open, write_failures) = sink.finish();
    let mut written_off = Vec::new();
    for (pos, message) in write_failures {
        let i = targets[pos].0;
        failed.push((i, message));
        note_failure(stats, i, &mut written_off);
    }

    for (pos, (i, final_path, temp_path)) in targets.iter().enumerate() {
        if !open[pos] {
            let _ = fs::remove_file(temp_path).await;
            continue;
        }
        if let Err(e) = fs::rename(temp_path, final_path).await {
            failed.push((*i, e.to_string()));
            note_failure(stats, *i, &mut written_off);
            let _ = fs::remove_file(temp_path).await;
            continue;
        }
        if preserve_mtime {
            let path = final_path.clone();
            let mtime = item.mtime;
            let applied = tokio::task::spawn_blocking(move || {
                filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(mtime, 0))
            })
            .await;
            match applied {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failed.push((*i, e.to_string())),
                Err(e) => failed.push((*i, e.to_string())),
            }
        }
        stats.dests[*i].files.fetch_add(1, Ordering::Relaxed);
        stats.dests[*i].bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    for i in written_off {
        failed.push((
            i,
            format!("giving up on this destination after {DEST_FAILURE_LIMIT} failures"),
        ));
    }
    Ok(FetchOutcome { bytes, failed })
}

/// Count a failure against a destination, noting when it is written off.
fn note_failure(stats: &Arc<Stats>, dest: usize, written_off: &mut Vec<usize>) {
    if stats.dests[dest].note_failure() {
        written_off.push(dest);
    }
}

/// How often the live counters are sampled for the timeline.
const PROGRESS_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// Writes each chunk to every destination, counting the bytes once.
///
/// Bytes are counted as they land rather than when a file completes: counting
/// on completion would make both the progress display and the recorded
/// timeline move in whole-file steps, which for multi-megabyte files means
/// minutes of apparent stillness.
///
/// A destination that errors is dropped from the set and its reason recorded;
/// the remaining ones carry on. Only when every destination has gone does the
/// write itself fail.
///
/// The chunk is stashed on the first call so that a `Pending` return can be
/// resumed where it left off, since the destinations do not accept it in step.
struct Fanout {
    files: Vec<Option<fs::File>>,
    failures: Vec<(usize, String)>,
    pending: Vec<u8>,
    at: usize,
    off: usize,
    counter: Arc<AtomicU64>,
    written: u64,
}

impl Fanout {
    fn new(files: Vec<Option<fs::File>>, counter: Arc<AtomicU64>) -> Self {
        Self {
            files,
            failures: Vec::new(),
            pending: Vec::new(),
            at: 0,
            off: 0,
            counter,
            written: 0,
        }
    }

    fn written(&self) -> u64 {
        self.written
    }

    fn all_closed(&self) -> bool {
        self.files.iter().all(Option::is_none)
    }

    fn fail(&mut self, pos: usize, message: String) {
        self.files[pos] = None;
        self.failures.push((pos, message));
    }

    /// Which destinations are still open, and why the others are not.
    fn finish(self) -> (Vec<bool>, Vec<(usize, String)>) {
        (
            self.files.iter().map(Option::is_some).collect(),
            self.failures,
        )
    }

    fn gone() -> std::io::Error {
        std::io::Error::other("every destination failed")
    }
}

impl tokio::io::AsyncWrite for Fanout {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        if this.pending.is_empty() {
            this.pending.extend_from_slice(data);
            this.at = 0;
            this.off = 0;
        }

        while this.at < this.files.len() {
            let pos = this.at;
            if this.files[pos].is_none() || this.off >= this.pending.len() {
                this.at += 1;
                this.off = 0;
                continue;
            }
            let outcome = {
                let chunk = &this.pending[this.off..];
                let file = this.files[pos].as_mut().expect("checked just above");
                Pin::new(file).poll_write(cx, chunk)
            };
            match outcome {
                Poll::Ready(Ok(0)) => this.fail(pos, "write returned zero bytes".to_string()),
                Poll::Ready(Ok(n)) => this.off += n,
                Poll::Ready(Err(e)) => this.fail(pos, e.to_string()),
                Poll::Pending => return Poll::Pending,
            }
        }

        if this.all_closed() {
            return Poll::Ready(Err(Self::gone()));
        }
        let n = this.pending.len();
        this.pending.clear();
        this.counter.fetch_add(n as u64, Ordering::Relaxed);
        this.written += n as u64;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        for pos in 0..this.files.len() {
            if this.files[pos].is_none() {
                continue;
            }
            let outcome = {
                let file = this.files[pos].as_mut().expect("checked just above");
                Pin::new(file).poll_flush(cx)
            };
            match outcome {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => this.fail(pos, e.to_string()),
                Poll::Pending => return Poll::Pending,
            }
        }
        if this.all_closed() {
            return Poll::Ready(Err(Self::gone()));
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.as_mut().poll_flush(cx)
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
    /// Below this many files the median is too noisy to be worth quoting.
    const MIN_SAMPLES: usize = 20;
    /// The estimate is taken from this share of the run's smallest files.
    const SMALLEST_SHARE: usize = 10;
    /// Refuse once byte time is this much of the observed duration: past it the
    /// answer is a small difference between two large numbers, which is noise.
    ///
    /// This is what rules out a run with no spread in file size, whether the
    /// files are all large or all tiny. With one size the measured rate already
    /// contains the fixed cost, so subtracting byte time removes everything and
    /// the two terms are not separately identifiable.
    const MAX_BYTE_SHARE: f64 = 0.5;

    let stream_time: f64 = samples.iter().map(|s| s.duration.as_secs_f64()).sum();
    let bytes: u64 = samples.iter().map(|s| s.size).sum();
    let rate = if stream_time > 0.0 {
        bytes as f64 / stream_time
    } else {
        0.0
    };

    if samples.len() < MIN_SAMPLES || rate <= 0.0 {
        return CostModel {
            fixed: None,
            basis: None,
            rate,
        };
    }

    // Work from the smallest files in the run rather than a fixed size cutoff:
    // what counts as "small enough that bytes hardly matter" depends on how
    // fast the link is, and a hard threshold silently refuses to answer on a
    // corpus of, say, 8 KiB files that would have measured perfectly well.
    //
    // Select by a size *threshold* and keep every file at or under it. Taking a
    // fixed count instead biases the sample whenever many files share the
    // smallest size: the sort is stable, so the slice becomes "the ones that
    // ran first", and those are exactly the ones that queued behind the large
    // transfers still saturating the link. Measured on a real mixed run that
    // mistake inflated the median from 13 ms to 788 ms.
    let mut sizes: Vec<u64> = samples.iter().map(|s| s.size).collect();
    sizes.sort_unstable();
    let cut = (samples.len() / SMALLEST_SHARE)
        .max(MIN_SAMPLES)
        .min(sizes.len());
    let threshold = sizes[cut - 1];
    let group: Vec<&Sample> = samples.iter().filter(|s| s.size <= threshold).collect();
    if group.len() < MIN_SAMPLES {
        return CostModel {
            fixed: None,
            basis: None,
            rate,
        };
    }

    let mut durations: Vec<Duration> = group.iter().map(|s| s.duration).collect();
    durations.sort_unstable();
    let median = durations[durations.len() / 2];

    let mean_bytes = group.iter().map(|s| s.size).sum::<u64>() as f64 / group.len() as f64;
    let byte_time = mean_bytes / rate;
    if byte_time > median.as_secs_f64() * MAX_BYTE_SHARE {
        return CostModel {
            fixed: None,
            basis: None,
            rate,
        };
    }

    CostModel {
        fixed: Some(median.saturating_sub(Duration::from_secs_f64(byte_time))),
        basis: Some((group.len(), group.last().map(|s| s.size).unwrap_or(0))),
        rate,
    }
}

struct CostModel {
    fixed: Option<Duration>,
    /// Number of files the estimate came from, and the largest of them.
    basis: Option<(usize, u64)>,
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
        let (count, largest) = cost.basis.expect("basis reported");
        assert!(count >= 20);
        assert_eq!(largest, 1024);
    }

    #[test]
    fn a_fixed_size_cutoff_does_not_gate_the_estimate() {
        // 8 KiB files: far above the old hard-coded 4 KiB threshold, but still
        // small enough relative to the rate for the estimate to hold.
        let samples: Vec<Sample> = (0..40)
            .map(|_| sample(8192, Duration::from_millis(40)))
            .chain((1..=4).map(|mib: u64| sample(mib << 20, Duration::from_secs(mib))))
            .collect();
        let cost = measure_cost(&samples);
        let fixed = cost.fixed.expect("8 KiB files are measurable");
        assert!(fixed.as_secs_f64() > 0.03, "{fixed:?}");
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
        // Uniformly large files: nearly all of each duration is bytes, so what
        // is left after subtracting them is noise rather than a measurement.
        let samples: Vec<Sample> = (1..=40)
            .map(|i: u64| sample(18 << 20, Duration::from_secs_f64(20.0 + i as f64 * 0.1)))
            .collect();
        let cost = measure_cost(&samples);
        assert!(cost.fixed.is_none());
        assert!(cost.basis.is_none());
        assert!(cost.rate > 0.0);
    }

    #[test]
    fn ties_at_the_smallest_size_do_not_bias_the_sample() {
        // Many files share the smallest size. The ones transferred first ran
        // while large files still saturated the link, so their durations are an
        // order of magnitude worse. Selecting a fixed count from a stable sort
        // would pick exactly those; selecting by size must keep them all.
        let mut samples: Vec<Sample> = (0..70)
            .map(|_| sample(4096, Duration::from_millis(788)))
            .collect();
        samples.extend((0..606).map(|_| sample(4096, Duration::from_millis(13))));
        samples.extend((0..25).map(|_| sample(64 << 20, Duration::from_secs(30))));

        let fixed = measure_cost(&samples).fixed.expect("measurable");
        // The median across all 676 small files is 13 ms, not the 788 ms of the
        // contended head of the run.
        assert!(fixed.as_secs_f64() < 0.05, "{fixed:?}");
    }

    #[test]
    fn refuses_when_there_are_too_few_files_to_be_sure() {
        let samples: Vec<Sample> = (0..5)
            .map(|_| sample(1024, Duration::from_millis(50)))
            .collect();
        assert!(measure_cost(&samples).fixed.is_none());
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
            fixed_basis: None,
            per_stream_rate: 1.0,
            streams: 8,
            samples: Vec::new(),
            progress: Vec::new(),
            dests: Vec::new(),
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
            fixed_basis: None,
            per_stream_rate: 1.0,
            streams: 8,
            samples: Vec::new(),
            progress: Vec::new(),
            dests: Vec::new(),
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
