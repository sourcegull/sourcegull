//! Counting computed values, for Trends: the capture-group series
//! (`mapping-compute`, one series per captured value).
//!
//! Counts decompose per file: [`Command::count_file`] counts one file's
//! values, so incremental backfill can update a total with
//! `count(new) = count(prev) − Σ count(old file versions) + Σ count(new
//! versions)` ([`ValueCounts::add`], [`ValueCounts::subtract`]).

use std::collections::BTreeMap;

use crate::command::{Command, MatchPattern, OutputKind};
use crate::query::{ComputeError, RegexpSite};
use crate::template::{MetaEnv, Template};

/// What a series groups its matches by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupBy {
    /// The first capture group's value.
    Capture,
}

impl GroupBy {
    /// The output template that produces the group's value.
    fn template(self) -> &'static str {
        match self {
            GroupBy::Capture => "$1",
        }
    }
}

/// Number of matches per value, ordered by value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValueCounts(pub BTreeMap<String, u64>);

impl ValueCounts {
    /// Counts one occurrence of `value` (empty values are not counted).
    pub fn insert(&mut self, value: &str) {
        if !value.is_empty() {
            *self.0.entry(value.to_owned()).or_default() += 1;
        }
    }

    pub fn add(&mut self, other: &ValueCounts) {
        for (k, v) in &other.0 {
            *self.0.entry(k.clone()).or_default() += v;
        }
    }

    /// Removes `other`'s counts; values reaching zero are dropped.
    pub fn subtract(&mut self, other: &ValueCounts) {
        for (k, v) in &other.0 {
            if let Some(n) = self.0.get_mut(k) {
                *n = n.saturating_sub(*v);
                if *n == 0 {
                    self.0.remove(k);
                }
            }
        }
    }

    pub fn total(&self) -> u64 {
        self.0.values().sum()
    }

    pub fn get(&self, value: &str) -> u64 {
        self.0.get(value).copied().unwrap_or(0)
    }
}

impl Command {
    /// The command an insight series runs: output `group_by`'s value for
    /// each match of the regexp `pattern` (case-insensitive unless
    /// `case_sensitive`).
    pub fn group_by(
        pattern: &str,
        case_sensitive: bool,
        group_by: GroupBy,
    ) -> Result<Command, ComputeError> {
        let regex = regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|e| ComputeError::Regexp(RegexpSite::Output, e.to_string()))?;
        Ok(Command::Output {
            pattern: MatchPattern::Regexp {
                source: pattern.to_owned(),
                regex,
            },
            template: Template::parse(group_by.template()),
            separator: "\n".to_owned(),
            type_value: None,
            selector: None,
            kind: OutputKind::Extra,
        })
    }

    /// Counts the values of one file (or blob) at one version: `meta`
    /// describes it (repository, path, language; author and date for
    /// commit-based series) and `content` is its text.
    pub fn count_file(&self, meta: &MetaEnv, content: &str) -> ValueCounts {
        let mut counts = ValueCounts::default();
        for v in self.file_values(meta, content) {
            counts.insert(&v);
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = "TODO(alice) x\nTODO(bob)\nTODO(alice)\n";
    const V2: &str = "TODO(alice)\nTODO(carol)\n";

    #[test]
    fn capture_counts_decompose_per_file() {
        let cmd = Command::group_by(r"TODO\((\w+)\)", false, GroupBy::Capture).unwrap();
        let meta = MetaEnv::default();
        let a = cmd.count_file(&meta, V1);
        assert_eq!(a.get("alice"), 2);
        assert_eq!(a.get("bob"), 1);
        assert_eq!(a.total(), 3);
        // Incremental update: the file changed from V1 to V2.
        let mut total = a.clone();
        total.add(&cmd.count_file(&meta, "todo(dan)\n"));
        total.subtract(&cmd.count_file(&meta, V1));
        total.add(&cmd.count_file(&meta, V2));
        let mut full = cmd.count_file(&meta, "todo(dan)\n");
        full.add(&cmd.count_file(&meta, V2));
        assert_eq!(total, full);
        assert!(!total.0.contains_key("bob"));
    }
}
