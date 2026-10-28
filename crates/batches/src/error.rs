//! Validation errors of bulk change specs and changeset specs.

use std::fmt;

pub use crate::yaml::Location;

/// One problem found in a spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecIssue {
    /// What is wrong, prefixed with the field in dotted notation (`on.0.branch: …`)
    /// unless it is about the whole document.
    pub message: String,
    /// JSON pointer of the node the issue is about (`""` for the document).
    pub path: String,
    /// Where that node is in the source text, when known.
    pub location: Option<Location>,
}

impl SpecIssue {
    pub(crate) fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        SpecIssue {
            message: message.into(),
            path: path.into(),
            location: None,
        }
    }

    pub(crate) fn at(mut self, location: Option<Location>) -> Self {
        self.location = location;
        self
    }
}

impl fmt::Display for SpecIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.location {
            Some(l) => write!(f, "{l}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// A spec that failed to parse or validate: every issue found, in source order.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct SpecErrors(pub Vec<SpecIssue>);

impl SpecErrors {
    pub fn issues(&self) -> &[SpecIssue] {
        &self.0
    }

    pub(crate) fn single(issue: SpecIssue) -> Self {
        SpecErrors(vec![issue])
    }
}

impl fmt::Display for SpecErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.as_slice() {
            [] => f.write_str("invalid spec"),
            [one] => write!(f, "{one}"),
            many => {
                write!(f, "{} errors occurred:", many.len())?;
                for issue in many {
                    write!(f, "\n\t* {issue}")?;
                }
                Ok(())
            }
        }
    }
}

/// The dotted field notation for a JSON pointer: `/on/0/branch` → `on.0.branch`.
pub(crate) fn field_name(pointer: &str) -> String {
    pointer
        .split('/')
        .skip(1)
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>()
        .join(".")
}

/// `field: message`, or just the message for the document root.
pub(crate) fn with_field(pointer: &str, message: &str) -> String {
    if pointer.is_empty() {
        message.to_string()
    } else {
        format!("{}: {message}", field_name(pointer))
    }
}

/// Sorts issues by position (unknown positions last) and drops exact duplicates.
pub(crate) fn sort_issues(issues: &mut Vec<SpecIssue>) {
    issues.sort_by(|a, b| {
        let ka = (a.location.is_none(), a.location, &a.path, &a.message);
        let kb = (b.location.is_none(), b.location, &b.path, &b.message);
        ka.cmp(&kb)
    });
    issues.dedup();
}
