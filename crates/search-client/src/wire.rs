//! Wire forms of the parts of a search: patterns, file filters and commit filters.

use sourcegull_rpc::repo::v1 as pb;
use sourcegull_search::{FileFilter, PatternSpec};

/// The planner's `CommitFilter` as commit and diff searches send it, with times as epoch
/// seconds; repo nodes compile it into regexps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitFilterSpec {
    pub authors: Vec<String>,
    pub exclude_authors: Vec<String>,
    pub committers: Vec<String>,
    pub exclude_committers: Vec<String>,
    pub messages: Vec<String>,
    pub exclude_messages: Vec<String>,
    pub before: Option<i64>,
    pub after: Option<i64>,
}

impl From<&sourcegull_search::CommitFilter> for CommitFilterSpec {
    fn from(f: &sourcegull_search::CommitFilter) -> Self {
        CommitFilterSpec {
            authors: f.authors.clone(),
            exclude_authors: f.exclude_authors.clone(),
            committers: f.committers.clone(),
            exclude_committers: f.exclude_committers.clone(),
            messages: f.messages.clone(),
            exclude_messages: f.exclude_messages.clone(),
            before: f.before.map(|t| t.timestamp()),
            after: f.after.map(|t| t.timestamp()),
        }
    }
}

/// Converts a spec to its wire form.
pub fn spec_to_pb(spec: &PatternSpec) -> pb::PatternNode {
    use pb::pattern_node::{Atom, List, Node, Structural};
    let list = |ss: &[PatternSpec]| List {
        children: ss.iter().map(spec_to_pb).collect(),
    };
    pb::PatternNode {
        node: Some(match spec {
            PatternSpec::Atom { regex, negated } => Node::Atom(Atom {
                regex: regex.clone(),
                negated: *negated,
            }),
            PatternSpec::Structural { template, rule } => Node::Structural(Structural {
                template: template.clone(),
                rule: rule.clone().unwrap_or_default(),
            }),
            PatternSpec::And(ss) => Node::And(list(ss)),
            PatternSpec::Or(ss) => Node::Or(list(ss)),
        }),
    }
}

/// Converts a wire pattern back; an empty node is an empty AND (true).
pub fn spec_from_pb(node: &pb::PatternNode) -> PatternSpec {
    use pb::pattern_node::Node;
    let list = |l: &pb::pattern_node::List| l.children.iter().map(spec_from_pb).collect();
    match &node.node {
        Some(Node::Atom(a)) => PatternSpec::Atom {
            regex: a.regex.clone(),
            negated: a.negated,
        },
        Some(Node::Structural(s)) => PatternSpec::Structural {
            template: s.template.clone(),
            rule: (!s.rule.is_empty()).then(|| s.rule.clone()),
        },
        Some(Node::And(l)) => PatternSpec::And(list(l)),
        Some(Node::Or(l)) => PatternSpec::Or(list(l)),
        None => PatternSpec::And(Vec::new()),
    }
}

/// The wire form of a file filter (predicates are evaluated by `web`).
pub fn path_filter_to_pb(f: &FileFilter) -> pb::PathFilter {
    pb::PathFilter {
        include: f.include.clone(),
        exclude: f.exclude.clone(),
        langs: f.langs.clone(),
        exclude_langs: f.exclude_langs.clone(),
    }
}

/// Reads the wire form of a file filter.
pub fn path_filter_from_pb(f: pb::PathFilter) -> FileFilter {
    FileFilter {
        include: f.include,
        exclude: f.exclude,
        langs: f.langs,
        exclude_langs: f.exclude_langs,
        predicates: Vec::new(),
    }
}

/// The wire form of commit filters.
pub fn commit_filters_to_pb(f: &CommitFilterSpec) -> pb::CommitFilters {
    pb::CommitFilters {
        authors: f.authors.clone(),
        exclude_authors: f.exclude_authors.clone(),
        committers: f.committers.clone(),
        exclude_committers: f.exclude_committers.clone(),
        messages: f.messages.clone(),
        exclude_messages: f.exclude_messages.clone(),
        before: f.before,
        after: f.after,
    }
}

/// Reads the wire form of commit filters.
pub fn commit_filters_from_pb(f: pb::CommitFilters) -> CommitFilterSpec {
    CommitFilterSpec {
        authors: f.authors,
        exclude_authors: f.exclude_authors,
        committers: f.committers,
        exclude_committers: f.exclude_committers,
        messages: f.messages,
        exclude_messages: f.exclude_messages,
        before: f.before,
        after: f.after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_round_trips() {
        let spec = PatternSpec::And(vec![
            PatternSpec::Atom {
                regex: "a".into(),
                negated: false,
            },
            PatternSpec::Or(vec![
                PatternSpec::Atom {
                    regex: "b".into(),
                    negated: true,
                },
                PatternSpec::Structural {
                    template: "f(:[x])".into(),
                    rule: Some("where :[x] == \"1\"".into()),
                },
            ]),
        ]);
        assert_eq!(spec_from_pb(&spec_to_pb(&spec)), spec);
    }
}
