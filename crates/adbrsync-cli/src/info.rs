//! Parsing for rsync's `--info=FLAGS`.

/// Which informational outputs are switched on, and how loudly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InfoFlags {
    /// 0 off, 2 whole-transfer progress. Level 1 is folded into 2; see `HELP`.
    pub progress: u8,
    pub stats: u8,
    pub name: u8,
}

impl InfoFlags {
    pub fn wants_progress(&self) -> bool {
        self.progress > 0
    }
    pub fn wants_stats(&self) -> bool {
        self.stats > 0
    }
    pub fn wants_names(&self) -> bool {
        self.name > 0
    }
}

pub const HELP: &str = "\
Supported --info flags (rsync syntax: comma separated, optional level digit):

  progress2   whole-transfer progress: bytes, percent, rate, ETA, file counts
  progress1   per-file progress -- not meaningful here, treated as progress2
              (files move on many concurrent streams, so there is no single
              current file to draw a bar for)
  name1       list each file that will be transferred
  stats1      print the summary block when finished
  all         progress2, name1 and stats1
  none        turn everything off
  help        print this list

Levels other than those listed are accepted and clamped. Unrecognised flag
names are reported and ignored.";

#[derive(Debug, PartialEq, Eq)]
pub enum ParseOutcome {
    /// `--info=help` was asked for.
    Help,
    Flags {
        flags: InfoFlags,
        /// Names that were not recognised, to be reported to the user.
        unknown: Vec<String>,
        /// True when `progress1` was requested and promoted to `progress2`.
        progress_promoted: bool,
    },
}

/// Parse an `--info` specification.
pub fn parse(spec: &str) -> ParseOutcome {
    let mut flags = InfoFlags::default();
    let mut unknown = Vec::new();
    let mut progress_promoted = false;

    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if item.eq_ignore_ascii_case("help") {
            return ParseOutcome::Help;
        }
        let (name, level) = split_level(item);
        match name.to_ascii_lowercase().as_str() {
            "all" => {
                flags = InfoFlags {
                    progress: 2,
                    stats: 1,
                    name: 1,
                }
            }
            "none" => flags = InfoFlags::default(),
            "progress" => {
                let level = level.unwrap_or(1);
                if level == 1 {
                    progress_promoted = true;
                }
                flags.progress = if level == 0 { 0 } else { 2 };
            }
            "stats" => flags.stats = level.unwrap_or(1).min(1),
            "name" => flags.name = level.unwrap_or(1).min(1),
            _ => unknown.push(name.to_string()),
        }
    }

    ParseOutcome::Flags {
        flags,
        unknown,
        progress_promoted,
    }
}

/// Split a trailing level digit off a flag name, as in `progress2`.
fn split_level(item: &str) -> (&str, Option<u8>) {
    let digits = item.len() - item.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return (item, None);
    }
    let split = item.len() - digits;
    (&item[..split], item[split..].parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags_of(spec: &str) -> InfoFlags {
        match parse(spec) {
            ParseOutcome::Flags { flags, .. } => flags,
            ParseOutcome::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn parses_progress2() {
        assert_eq!(flags_of("progress2").progress, 2);
        assert!(flags_of("progress2").wants_progress());
    }

    #[test]
    fn progress1_is_promoted_to_progress2() {
        match parse("progress1") {
            ParseOutcome::Flags {
                flags,
                progress_promoted,
                ..
            } => {
                assert_eq!(flags.progress, 2);
                assert!(progress_promoted);
            }
            ParseOutcome::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn bare_progress_behaves_like_level_one() {
        match parse("progress") {
            ParseOutcome::Flags {
                flags,
                progress_promoted,
                ..
            } => {
                assert_eq!(flags.progress, 2);
                assert!(progress_promoted);
            }
            ParseOutcome::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn progress0_turns_it_off() {
        assert_eq!(flags_of("progress0").progress, 0);
    }

    #[test]
    fn combines_comma_separated_flags() {
        let f = flags_of("progress2,stats1,name1");
        assert!(f.wants_progress() && f.wants_stats() && f.wants_names());
    }

    #[test]
    fn all_and_none_are_shorthands() {
        assert_eq!(
            flags_of("all"),
            InfoFlags {
                progress: 2,
                stats: 1,
                name: 1
            }
        );
        assert_eq!(flags_of("all,none"), InfoFlags::default());
    }

    #[test]
    fn help_short_circuits() {
        assert_eq!(parse("progress2,help"), ParseOutcome::Help);
    }

    #[test]
    fn unknown_names_are_collected_not_fatal() {
        match parse("progress2,nosuchflag") {
            ParseOutcome::Flags { flags, unknown, .. } => {
                assert_eq!(flags.progress, 2);
                assert_eq!(unknown, vec!["nosuchflag".to_string()]);
            }
            ParseOutcome::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn splits_the_level_digit_off_a_name() {
        assert_eq!(split_level("progress2"), ("progress", Some(2)));
        assert_eq!(split_level("name"), ("name", None));
        assert_eq!(split_level("stats10"), ("stats", Some(10)));
    }
}
