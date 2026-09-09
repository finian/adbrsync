use globset::{Glob, GlobSet, GlobSetBuilder};

/// Include/exclude rules, applied in the order they were given like rsync's.
#[derive(Debug, Default)]
pub struct Filter {
    includes: Option<GlobSet>,
    excludes: Option<GlobSet>,
}

#[derive(Debug, thiserror::Error)]
#[error("bad filter pattern {pattern:?}: {source}")]
pub struct FilterError {
    pattern: String,
    #[source]
    source: globset::Error,
}

impl Filter {
    pub fn new(includes: &[String], excludes: &[String]) -> Result<Self, FilterError> {
        Ok(Self {
            includes: build(includes)?,
            excludes: build(excludes)?,
        })
    }

    /// True when the path should be transferred.
    ///
    /// An include match always wins, matching rsync's rule that the first
    /// matching pattern decides and includes are consulted first.
    pub fn accepts(&self, rel: &str) -> bool {
        if let Some(inc) = &self.includes {
            if inc.is_match(rel) {
                return true;
            }
        }
        match &self.excludes {
            Some(exc) => !exc.is_match(rel),
            None => true,
        }
    }
}

fn build(patterns: &[String]) -> Result<Option<GlobSet>, FilterError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        // A bare name like `*.tmp` should match at any depth, the way rsync
        // treats a pattern without a slash.
        let expanded = if pattern.contains('/') {
            pattern.trim_start_matches('/').to_string()
        } else {
            format!("**/{pattern}")
        };
        let glob = Glob::new(&expanded).map_err(|source| FilterError {
            pattern: pattern.clone(),
            source,
        })?;
        builder.add(glob);
        // `**/x` does not match a top-level `x` in globset, so add it plainly too.
        if !pattern.contains('/') {
            if let Ok(g) = Glob::new(pattern) {
                builder.add(g);
            }
        }
    }
    builder.build().map(Some).map_err(|source| FilterError {
        pattern: patterns.join(","),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(inc: &[&str], exc: &[&str]) -> Filter {
        let inc: Vec<String> = inc.iter().map(|s| s.to_string()).collect();
        let exc: Vec<String> = exc.iter().map(|s| s.to_string()).collect();
        Filter::new(&inc, &exc).unwrap()
    }

    #[test]
    fn accepts_everything_without_rules() {
        assert!(filter(&[], &[]).accepts("any/path.txt"));
    }

    #[test]
    fn excludes_by_bare_name_at_any_depth() {
        let f = filter(&[], &["*.tmp"]);
        assert!(!f.accepts("a.tmp"));
        assert!(!f.accepts("deep/nested/a.tmp"));
        assert!(f.accepts("a.txt"));
    }

    #[test]
    fn excludes_by_path_prefix() {
        let f = filter(&[], &["Android/**"]);
        assert!(!f.accepts("Android/data/x"));
        assert!(f.accepts("DCIM/x"));
    }

    #[test]
    fn include_overrides_exclude() {
        let f = filter(&["keep/*.tmp"], &["*.tmp"]);
        assert!(f.accepts("keep/a.tmp"));
        assert!(!f.accepts("other/a.tmp"));
    }
}
