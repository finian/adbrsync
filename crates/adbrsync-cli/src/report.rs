use adbrsync_core::plan::{Plan, Reason};
use adbrsync_core::scan::RemoteScan;
use adbrsync_core::transfer::TransferReport;

/// Console output, gated on verbosity.
pub struct Printer {
    quiet: bool,
    human: bool,
}

impl Printer {
    pub fn new(quiet: bool, human: bool) -> Self {
        Self { quiet, human }
    }

    pub fn info(&self, msg: &str) {
        if !self.quiet {
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
        println!(
            "{} to transfer ({}), {} unchanged, {} filtered, {} to delete",
            plan.transfers.len(),
            human_bytes(plan.total_bytes, self.human),
            plan.unchanged,
            plan.filtered,
            plan.deletions.len()
        );
        if plan.symlinks_skipped > 0 {
            println!(
                "{} symlinks skipped (not reproduced in this version)",
                plan.symlinks_skipped
            );
        }
    }

    pub fn transfer_list(&self, plan: &Plan) {
        if self.quiet {
            return;
        }
        for item in &plan.transfers {
            let reason = match plan.reasons.get(&item.rel) {
                Some(Reason::Missing) => "new",
                Some(Reason::SizeDiffers) => "size",
                Some(Reason::TimeDiffers) => "time",
                Some(Reason::ContentDiffers) => "hash",
                None => "?",
            };
            println!(
                "  {reason:<5} {:>10}  {}",
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
                    println!("  per-file fixed    {:.1} ms", fixed.as_secs_f64() * 1000.0);
                    println!("  fixed cost share  {:.1}% of wall time", share * 100.0);
                }
                _ => println!(
                    "  per-file fixed    not measurable (too few files under 4 KiB in this run)"
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
}
