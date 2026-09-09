use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adbrsync_core::transfer::Stats;
use tokio::task::JoinHandle;

const TICK: Duration = Duration::from_millis(200);
/// Weight given to the newest rate sample. Low enough that the displayed rate
/// does not jitter with every tick, high enough to follow a real change.
const SMOOTHING: f64 = 0.3;

/// Render whole-transfer progress, the way rsync's `--info=progress2` does.
///
/// There is deliberately no per-file variant: files move on many concurrent
/// streams at once, so there is no single "current file" to draw a bar for.
pub fn spawn(stats: Arc<Stats>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut ticker = tokio::time::interval(TICK);
        let mut last = (started, 0u64);
        let mut rate = 0.0f64;

        loop {
            ticker.tick().await;
            let now = Instant::now();
            let done = stats.bytes_done.load(Ordering::Relaxed);
            let total = stats.bytes_total.load(Ordering::Relaxed);
            let files_done = stats.files_done.load(Ordering::Relaxed);
            let files_total = stats.files_total.load(Ordering::Relaxed);

            let delta = now.duration_since(last.0).as_secs_f64();
            if delta > 0.0 {
                let sample = done.saturating_sub(last.1) as f64 / delta;
                rate = if rate == 0.0 {
                    sample
                } else {
                    SMOOTHING * sample + (1.0 - SMOOTHING) * rate
                };
                last = (now, done);
            }

            eprint!(
                "\r{}\x1b[K",
                line(done, total, files_done, files_total, rate)
            );
        }
    })
}

/// Clear the progress line once the transfer is over.
pub fn clear() {
    eprint!("\r\x1b[K");
}

fn line(done: u64, total: u64, files_done: u64, files_total: u64, rate: f64) -> String {
    let percent = if total > 0 {
        (done as f64 / total as f64 * 100.0).min(100.0)
    } else {
        0.0
    };
    let eta = if rate > 1.0 && total > done {
        duration((total - done) as f64 / rate)
    } else {
        "    0:00:00".trim().to_string()
    };
    format!(
        "{:>14}  {:>3.0}%  {:>10}/s  {:>8} (xfr#{}, to-chk={}/{})",
        thousands(done),
        percent,
        si(rate as u64),
        eta,
        files_done,
        files_total.saturating_sub(files_done),
        files_total,
    )
}

/// Group digits, as rsync does for the byte counter.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn si(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.2}{}", UNITS[unit])
    }
}

fn duration(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!(
        "{}:{:02}:{:02}",
        total / 3600,
        (total / 60) % 60,
        total % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_digits_in_threes() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(12_549_779_192), "12,549,779,192");
    }

    #[test]
    fn formats_durations_as_hours_minutes_seconds() {
        assert_eq!(duration(0.0), "0:00:00");
        assert_eq!(duration(65.0), "0:01:05");
        assert_eq!(duration(3_725.0), "1:02:05");
    }

    #[test]
    fn scales_rates() {
        assert_eq!(si(512), "512B");
        assert_eq!(si(1536), "1.50KB");
        assert_eq!(si(13_600_000), "12.97MB");
    }

    #[test]
    fn progress_line_reports_percent_and_remaining_counts() {
        let text = line(500, 1000, 3, 10, 100.0);
        assert!(text.contains("50%"), "{text}");
        assert!(text.contains("(xfr#3, to-chk=7/10)"), "{text}");
    }

    #[test]
    fn handles_an_empty_transfer_without_dividing_by_zero() {
        let text = line(0, 0, 0, 0, 0.0);
        assert!(text.contains("0%"), "{text}");
        assert!(text.contains("to-chk=0/0"), "{text}");
    }

    #[test]
    fn percent_never_exceeds_one_hundred() {
        let text = line(1500, 1000, 1, 1, 10.0);
        assert!(text.contains("100%"), "{text}");
    }
}
