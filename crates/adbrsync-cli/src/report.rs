use std::path::Path;

use adbrsync_core::plan::{Plan, Reason};
use adbrsync_core::scan::RemoteScan;
use adbrsync_core::transfer::TransferReport;

/// Console output, gated on verbosity.
pub struct Printer {
    quiet: bool,
    verbose: bool,
    human: bool,
}

impl Printer {
    pub fn new(quiet: bool, verbose: bool, human: bool) -> Self {
        Self {
            quiet,
            verbose,
            human,
        }
    }

    pub fn info(&self, msg: &str) {
        if !self.quiet {
            println!("{msg}");
        }
    }

    /// Background the user did not ask for. It prints only under -v, so that
    /// an error stands alone instead of trailing a line about something that
    /// worked.
    pub fn detail(&self, msg: &str) {
        if self.verbose && !self.quiet {
            println!("{msg}");
        }
    }

    pub fn warn(&self, msg: &str) {
        eprintln!("adbrsync: {msg}");
    }

    pub fn plan_summary(&self, remote: &RemoteScan, plan: &Plan) {
        if self.quiet {
            return;
        }
        println!(
            "scanned {} entries under {}",
            remote.entries.len(),
            remote.root
        );

        // With one destination there is nothing to disambiguate, so keep the
        // familiar single line.
        if plan.dests.len() <= 1 {
            let dest = plan.dests.first();
            println!(
                "{} to transfer ({}), {} unchanged, {} filtered, {} to delete",
                plan.transfers.len(),
                human_bytes(plan.total_bytes, self.human),
                dest.map(|d| d.unchanged).unwrap_or(0),
                plan.filtered,
                dest.map(|d| d.deletions.len()).unwrap_or(0),
            );
        } else {
            // The headline is what comes off the device, since that is the
            // scarce resource; each destination's own share follows.
            println!(
                "{} files to read from the device ({}), {} unchanged everywhere, {} filtered",
                plan.transfers.len(),
                human_bytes(plan.total_bytes, self.human),
                plan.unchanged_everywhere(),
                plan.filtered,
            );
            for (i, dest) in plan.dests.iter().enumerate() {
                println!(
                    "  [{}] {:<30} {} files, {}, {} unchanged, {} to delete",
                    i + 1,
                    shorten(&dest.root),
                    dest.files,
                    human_bytes(dest.bytes, self.human),
                    dest.unchanged,
                    dest.deletions.len(),
                );
            }
        }

        if plan.symlinks_skipped > 0 {
            println!(
                "{} symlinks skipped (not reproduced in this version)",
                plan.symlinks_skipped
            );
        }
    }

    pub fn transfer_list(&self, plan: &Plan, dests: &[std::path::PathBuf]) {
        if self.quiet {
            return;
        }
        let many = dests.len() > 1;
        for item in &plan.transfers {
            let reason = match plan.reasons.get(&item.rel) {
                Some(Reason::Missing) => "new",
                Some(Reason::SizeDiffers) => "size",
                Some(Reason::TimeDiffers) => "time",
                Some(Reason::ContentDiffers) => "hash",
                None => "?",
            };
            // Only name the destinations when there is a choice to report.
            let targets = if many {
                let list: Vec<String> = item.targets.iter().map(|i| (i + 1).to_string()).collect();
                format!("  -> {}", list.join(","))
            } else {
                String::new()
            };
            println!(
                "  {reason:<5} {:>10}  {}{targets}",
                human_bytes(item.size, self.human),
                item.rel
            );
        }
    }

    pub fn final_report(&self, report: &TransferReport, stats: bool) {
        if !self.quiet {
            println!(
                "transferred {} files, {} in {:.2}s ({}/s)",
                report.files,
                human_bytes(report.bytes, self.human),
                report.elapsed.as_secs_f64(),
                human_bytes(report.throughput() as u64, self.human)
            );
            if report.dests.len() > 1 {
                for (i, dest) in report.dests.iter().enumerate() {
                    let failures = if dest.failures > 0 {
                        format!("  ({} failed)", dest.failures)
                    } else {
                        String::new()
                    };
                    println!(
                        "  [{}] {:<30} {} files, {}{failures}",
                        i + 1,
                        shorten(&dest.root),
                        dest.files,
                        human_bytes(dest.bytes, self.human),
                    );
                }
            }
        }
        if stats {
            println!("stats:");
            println!("  wall time         {:.3}s", report.elapsed.as_secs_f64());
            println!("  streams           {}", report.streams);
            println!(
                "  aggregate rate    {}/s",
                human_bytes(report.throughput() as u64, self.human)
            );
            println!(
                "  stream busy       {:.3}s ({:.1}% of available stream time)",
                report.stream_time.as_secs_f64(),
                report.stream_utilization() * 100.0
            );
            match (report.per_file_fixed, report.overhead_fraction()) {
                (Some(fixed), Some(share)) => {
                    let basis = match report.fixed_basis {
                        Some((count, largest)) => format!(
                            " (from {count} files up to {})",
                            human_bytes(largest, self.human)
                        ),
                        None => String::new(),
                    };
                    println!(
                        "  per-file fixed    {:.1} ms{basis}",
                        fixed.as_secs_f64() * 1000.0
                    );
                    println!("  fixed cost share  {:.1}% of wall time", share * 100.0);
                }
                _ => println!(
                    "  per-file fixed    not measurable \
                     (needs a spread of file sizes in one run; with a single size the \
                     fixed cost cannot be told apart from the bytes)"
                ),
            }
        }
        for e in &report.errors {
            eprintln!("adbrsync: {}: {}", e.rel, e.message);
        }
        if !report.errors.is_empty() {
            eprintln!("adbrsync: {} files failed", report.errors.len());
        }
    }
}

/// Keep a destination readable in a column without losing which one it is.
fn shorten(path: &Path) -> String {
    let full = path.display().to_string();
    if full.chars().count() <= 30 {
        return full;
    }
    let tail: String = full
        .chars()
        .rev()
        .take(27)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{tail}")
}

/// Format a byte count, either exactly or scaled for humans.
pub fn human_bytes(bytes: u64, human: bool) -> String {
    if !human {
        return bytes.to_string();
    }
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_byte_counts_when_not_human() {
        assert_eq!(human_bytes(1536, false), "1536");
    }

    #[test]
    fn scales_units_for_humans() {
        assert_eq!(human_bytes(512, true), "512B");
        assert_eq!(human_bytes(1536, true), "1.50K");
        assert_eq!(human_bytes(5 * 1024 * 1024, true), "5.00M");
    }

    #[test]
    fn short_paths_are_left_alone() {
        assert_eq!(shorten(Path::new("/Volumes/M1")), "/Volumes/M1");
    }

    #[test]
    fn long_paths_keep_their_tail() {
        let long = Path::new("/Volumes/Backup/2026/september/phone/DCIM/Camera");
        let out = shorten(long);
        assert!(out.starts_with("..."), "{out}");
        assert!(out.ends_with("Camera"), "{out}");
        assert_eq!(out.chars().count(), 30);
    }
}
