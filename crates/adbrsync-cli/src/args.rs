use clap::{ArgAction, Parser};

/// Pull files from an Android device over the ADB transport.
///
/// The interface follows rsync where Android's semantics allow it. The
/// differences that matter are listed under "Android notes" below.
#[derive(Debug, Parser)]
#[command(
    name = "adbrsync",
    version,
    about = "High-throughput Android backup over the ADB transport",
    disable_help_flag = true,
    after_help = ANDROID_NOTES
)]
pub struct Args {
    /// Source on the device, as `device:/path` or `<serial>:/path`.
    ///
    /// A trailing slash copies the contents of the directory; without one the
    /// directory itself is created inside DEST, exactly as in rsync.
    pub src: String,

    /// Local destination directories.
    ///
    /// Give more than one and the pull is written to all of them from a single
    /// read of the device, which is the cheapest way to keep two backup drives
    /// in step: the device link is the slow part, and a second local write is
    /// nearly free. Each destination is compared separately, so one that has
    /// fallen behind is repaired rather than left behind.
    #[arg(required = true, num_args = 1..)]
    pub dests: Vec<String>,

    /// Archive mode. On Android this means `-rt` plus symlink handling where
    /// the filesystem supports it; see the notes below.
    #[arg(short = 'a', long)]
    pub archive: bool,

    /// Recurse into directories.
    #[arg(short = 'r', long)]
    pub recursive: bool,

    /// Preserve modification times.
    #[arg(short = 't', long)]
    pub times: bool,

    /// Copy symlinks as symlinks.
    #[arg(short = 'l', long)]
    pub links: bool,

    /// Increase verbosity; repeat for more.
    #[arg(short = 'v', long, action = ArgAction::Count)]
    pub verbose: u8,

    /// Suppress non-error output.
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Show what would be done without transferring anything.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Delete local files that no longer exist on the device.
    #[arg(long)]
    pub delete: bool,

    /// Exclude files matching PATTERN.
    #[arg(long, value_name = "PATTERN")]
    pub exclude: Vec<String>,

    /// Read exclude patterns from FILE, one per line.
    #[arg(long = "exclude-from", value_name = "FILE")]
    pub exclude_from: Option<String>,

    /// Include files matching PATTERN even when excluded.
    #[arg(long, value_name = "PATTERN")]
    pub include: Vec<String>,

    /// Show whole-transfer progress. Equivalent to `--info=progress2`.
    #[arg(long)]
    pub progress: bool,

    /// Fine-grained output control, as in rsync. Use `--info=help` for the list.
    #[arg(long, value_name = "FLAGS")]
    pub info: Option<String>,

    /// Write a machine-readable performance report for this run to FILE (JSON).
    ///
    /// Records phase timings, a per-size-class breakdown of where transfer time
    /// went, a per-second completion timeline, and every raw per-file sample,
    /// so later tuning can be argued from data rather than intuition.
    #[arg(long = "perf-report", value_name = "FILE")]
    pub perf_report: Option<String>,

    /// Print a summary when finished.
    #[arg(long)]
    pub stats: bool,

    /// Compare by checksum rather than size and modification time.
    #[arg(short = 'c', long)]
    pub checksum: bool,

    /// Keep partially transferred files. Accepted for compatibility; the sync
    /// service cannot resume mid-file, so partial files are always discarded.
    #[arg(long)]
    pub partial: bool,

    /// Skip files larger than SIZE (suffixes K, M, G).
    #[arg(long = "max-size", value_name = "SIZE")]
    pub max_size: Option<String>,

    /// Skip files smaller than SIZE (suffixes K, M, G).
    #[arg(long = "min-size", value_name = "SIZE")]
    pub min_size: Option<String>,

    /// Request compressed transfer. Only brotli is offered by many devices and
    /// it loses on already-compressed media, so this is off by default.
    #[arg(short = 'z', long)]
    pub compress: bool,

    /// Output sizes in a human readable form.
    #[arg(short = 'h', long = "human-readable")]
    pub human_readable: bool,

    /// Create a destination directory that does not exist.
    ///
    /// Off by default on purpose: an unmounted drive leaves an empty mount
    /// point behind, and writing there fills the internal disk while looking
    /// like a successful backup.
    #[arg(long = "mkpath")]
    pub mkpath: bool,

    /// Carry on when a destination does not exist, instead of refusing to run.
    ///
    /// For a set of drives that are not all attached at once: the ones that are
    /// present get their backup, the missing ones are reported, and the run
    /// exits 23 to say it was partial. Without this a missing destination stops
    /// everything, on the grounds that it is usually a mistake.
    #[arg(long = "skip-missing-dest")]
    pub skip_missing_dest: bool,

    /// Number of concurrent sync streams.
    ///
    /// One sync stream cannot have two requests in flight, so this is the main
    /// lever against per-file latency. The default was measured on a wireless
    /// link and should be re-tuned over USB.
    #[arg(long, value_name = "N", default_value_t = 16)]
    pub streams: usize,

    /// Address of the adb server.
    #[arg(long, value_name = "ADDR", default_value = adb_proto::DEFAULT_SERVER_ADDR)]
    pub server: String,

    /// Treat modification times within SECONDS as equal.
    #[arg(long = "mtime-tolerance", value_name = "SECONDS", default_value_t = 1)]
    pub mtime_tolerance: i64,

    /// Accepted for rsync compatibility and ignored: the FUSE volume backing
    /// /sdcard synthesizes these values, so they cannot be read or restored.
    #[arg(short = 'p', long = "perms")]
    pub perms: bool,

    /// Ignored; see --perms.
    #[arg(short = 'o', long = "owner")]
    pub owner: bool,

    /// Ignored; see --perms.
    #[arg(short = 'g', long = "group")]
    pub group: bool,

    /// Print this help.
    #[arg(long, action = ArgAction::Help)]
    pub help: Option<bool>,
}

const ANDROID_NOTES: &str = "\
Android notes:
  -a means -rt here. The FUSE mount behind /sdcard synthesizes permissions,
  owner and group, so -p, -o and -g are accepted but ignored with a warning.

  Whole-file transfer is always used. rsync's delta algorithm would force a
  full device-side read of every candidate file to save transfers that do not
  happen on media-dominated backup sets.

  On Android 11 and later the adb shell user generally cannot read
  /sdcard/Android/data or /Android/obb. Those paths are reported as skipped,
  never silently dropped. Some devices mount them readably and are unaffected.

Examples:
  adbrsync -av --delete device:/sdcard/DCIM/ ./backup/
  adbrsync -av --exclude '*.tmp' <serial>:/sdcard/Pictures ./backup/
";

/// A source argument split into a device selector and a device path.
#[derive(Debug, PartialEq, Eq)]
pub struct RemoteSpec {
    /// `None` means "whatever single device is connected".
    pub serial: Option<String>,
    pub path: String,
    /// Whether the argument ended in a slash, which selects rsync's
    /// "contents of" semantics.
    pub trailing_slash: bool,
}

/// Split `device:/path`, `<serial>:/path` or `:/path`.
///
/// The separator is the last colon that introduces an absolute path, so a
/// wireless serial such as `192.168.0.108:41567` survives intact.
pub fn parse_remote(spec: &str) -> Option<RemoteSpec> {
    let idx = spec
        .match_indices(':')
        .filter(|(i, _)| spec[i + 1..].starts_with('/'))
        .map(|(i, _)| i)
        .next_back()?;
    let (head, path) = spec.split_at(idx);
    let path = &path[1..];
    let serial = match head {
        "" | "device" | "adb" => None,
        other => Some(other.to_string()),
    };
    Some(RemoteSpec {
        serial,
        trailing_slash: path.ends_with('/'),
        path: normalize(path),
    })
}

fn normalize(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Parse a size with an optional K/M/G suffix.
pub fn parse_size(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (digits, scale) = match raw.chars().last()?.to_ascii_uppercase() {
        'K' => (&raw[..raw.len() - 1], 1024f64),
        'M' => (&raw[..raw.len() - 1], 1024f64 * 1024.0),
        'G' => (&raw[..raw.len() - 1], 1024f64 * 1024.0 * 1024.0),
        'B' => (&raw[..raw.len() - 1], 1.0),
        _ => (raw, 1.0),
    };
    let value: f64 = digits.trim().parse().ok()?;
    (value >= 0.0).then_some((value * scale) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_prefix() {
        let r = parse_remote("device:/sdcard/DCIM/").unwrap();
        assert_eq!(r.serial, None);
        assert_eq!(r.path, "/sdcard/DCIM");
        assert!(r.trailing_slash);
    }

    #[test]
    fn parses_explicit_serial() {
        let r = parse_remote("ABC123:/sdcard").unwrap();
        assert_eq!(r.serial.as_deref(), Some("ABC123"));
        assert_eq!(r.path, "/sdcard");
        assert!(!r.trailing_slash);
    }

    #[test]
    fn keeps_the_port_of_a_wireless_serial() {
        let r = parse_remote("192.168.0.108:41567:/sdcard/DCIM").unwrap();
        assert_eq!(r.serial.as_deref(), Some("192.168.0.108:41567"));
        assert_eq!(r.path, "/sdcard/DCIM");
    }

    #[test]
    fn accepts_a_bare_colon_for_any_device() {
        let r = parse_remote(":/sdcard").unwrap();
        assert_eq!(r.serial, None);
        assert_eq!(r.path, "/sdcard");
    }

    #[test]
    fn rejects_local_paths() {
        assert!(parse_remote("./backup").is_none());
        assert!(parse_remote("/sdcard/DCIM").is_none());
        assert!(parse_remote("device:relative/path").is_none());
    }

    #[test]
    fn root_path_survives_normalization() {
        assert_eq!(parse_remote("device:/").unwrap().path, "/");
    }

    #[test]
    fn parses_sizes_with_suffixes() {
        assert_eq!(parse_size("100"), Some(100));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1.5M"), Some(1_572_864));
        assert_eq!(parse_size("2g"), Some(2_147_483_648));
        assert_eq!(parse_size("nope"), None);
    }
}
