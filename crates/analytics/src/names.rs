//! Usage event names and the groups the admin analytics count.
//!
//! Event names are stable identifiers (`SearchResultsQueried`, `ViewBlob`, `findReferences`,
//! `codeintel.lsifHover`, …): stored events and the analytics definitions depend on them. Server
//! events have the source [`BACKEND`]; the web app logs page views and clicks with `logEvent`.

/// Source of events written by the server.
pub const BACKEND: &str = "BACKEND";
/// Source of the browser extension's events.
pub const CODE_HOST_INTEGRATION: &str = "CODEHOSTINTEGRATION";

// Search.
/// A search ran (server, the streaming search API). Argument: `type`, `patternType`,
/// `durationMs`, `resultCount`.
pub const SEARCH_RESULTS_QUERIED: &str = "SearchResultsQueried";
/// A search result was clicked (web).
pub const SEARCH_RESULT_CLICKED: &str = "SearchResultClicked";
/// A file was viewed (web; also the recent views ownership signal).
pub const VIEW_BLOB: &str = "ViewBlob";
/// "Open on code host" was clicked (web).
pub const GO_TO_CODE_HOST: &str = "GoToCodeHostClicked";
/// Code was copied from a result or file (web).
pub const CODE_COPIED: &str = "CodeCopied";
/// A search from the VS Code extension.
pub const VSCODE_SEARCH: &str = "VSCESearchSubmitted";
/// A search from the JetBrains plugin.
pub const JETBRAINS_SEARCH: &str = "IDESearchSubmitted";

// Code navigation.
/// "Go to definition" was clicked (web).
pub const GO_TO_DEFINITION: &str = "goToDefinition";
/// "Go to definition" with a preloaded result (web).
pub const GO_TO_DEFINITION_PRELOADED: &str = "goToDefinition.preloaded";
/// "Find references" was clicked (web).
pub const FIND_REFERENCES: &str = "findReferences";
/// Prefix of the server's code navigation events (see [`codeintel_event`]).
pub const CODEINTEL_PREFIX: &str = "codeintel.";

// Notebooks.
pub const NOTEBOOK_CREATED: &str = "SearchNotebookCreated";
pub const NOTEBOOK_VIEWED: &str = "SearchNotebookPageViewed";
pub const NOTEBOOK_BLOCK_RUN: &str = "SearchNotebookBlockRun";

// Watches.
pub const MONITOR_CREATED: &str = "CodeMonitorCreated";
pub const MONITOR_LIST_VIEWED: &str = "CodeMonitoringPageViewed";
pub const MONITOR_VIEWED: &str = "CodeMonitorPageViewed";

// Bulk changes.
pub const BATCH_CHANGE_LIST_VIEWED: &str = "BatchChangeListPageViewed";
pub const BATCH_CHANGE_VIEWED: &str = "BatchChangeDetailsPageViewed";
pub const BATCH_CHANGE_APPLIED: &str = "BatchChangeApplied";

// Code insights.
pub const INSIGHT_ADDED: &str = "InsightAddition";
pub const INSIGHTS_VIEWED: &str = "ViewInsights";
pub const INSIGHT_HOVER: &str = "InsightHover";
pub const INSIGHT_DATA_POINT_CLICK: &str = "InsightDataPointClick";

// Accounts.
pub const SIGN_IN: &str = "SignInSucceeded";
pub const SIGN_UP: &str = "SignUpSucceeded";

/// The cookie holding a signed-out visitor's random id. The web app sets it; the server reads it
/// for its own events of anonymous users (`auth.public`).
pub const ANONYMOUS_UID_COOKIE: &str = "sourcegullAnonymousUid";

/// The anonymous visitor id in a `Cookie` header, if it is well-formed (1–64 characters of
/// `[A-Za-z0-9-_]`).
pub fn anonymous_uid_from_cookie(header: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == ANONYMOUS_UID_COOKIE
            && (1..=64).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .then(|| value.to_owned())
    })
}

/// A code navigation request answered by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeIntelAction {
    Hover,
    Definitions,
    References,
}

impl CodeIntelAction {
    fn suffix(self) -> &'static str {
        match self {
            CodeIntelAction::Hover => "Hover",
            CodeIntelAction::Definitions => "Definitions",
            CodeIntelAction::References => "References",
        }
    }

    /// The `kind` column of the top repositories table.
    pub fn kind(self) -> &'static str {
        match self {
            CodeIntelAction::Hover => "hover",
            CodeIntelAction::Definitions => "definitions",
            CodeIntelAction::References => "references",
        }
    }
}

/// The name of a server code navigation event: `codeintel.lsifHover` (precise),
/// `codeintel.searchReferences` (search-based or syntactic), with `.xrepo` when the results reach
/// into another repository.
pub fn codeintel_event(action: CodeIntelAction, precise: bool, cross_repo: bool) -> String {
    format!(
        "{CODEINTEL_PREFIX}{}{}{}",
        if precise { "lsif" } else { "search" },
        action.suffix(),
        if cross_repo { ".xrepo" } else { "" }
    )
}

/// Every name [`codeintel_event`] returns.
pub fn codeintel_events() -> Vec<String> {
    let mut out = Vec::new();
    for action in [
        CodeIntelAction::Hover,
        CodeIntelAction::Definitions,
        CodeIntelAction::References,
    ] {
        for precise in [true, false] {
            for xrepo in [false, true] {
                out.push(codeintel_event(action, precise, xrepo));
            }
        }
    }
    out
}

/// The server code navigation events that are precise (or not).
pub fn codeintel_events_by_precision(precise: bool) -> Vec<String> {
    codeintel_events()
        .into_iter()
        .filter(|n| is_precise(n) == precise)
        .collect()
}

/// The server code navigation events that reach into another repository.
pub fn codeintel_cross_repo_events() -> Vec<String> {
    codeintel_events()
        .into_iter()
        .filter(|n| n.ends_with(".xrepo"))
        .collect()
}

/// Whether a server code navigation event is precise.
pub fn is_precise(name: &str) -> bool {
    name.starts_with("codeintel.lsif")
}

/// The action of a server code navigation event.
pub fn codeintel_action(name: &str) -> Option<CodeIntelAction> {
    let rest = name.strip_prefix(CODEINTEL_PREFIX)?;
    let rest = rest.strip_suffix(".xrepo").unwrap_or(rest);
    let rest = rest
        .strip_prefix("lsif")
        .or_else(|| rest.strip_prefix("search"))?;
    match rest {
        "Hover" => Some(CodeIntelAction::Hover),
        "Definitions" => Some(CodeIntelAction::Definitions),
        "References" => Some(CodeIntelAction::References),
        _ => None,
    }
}

/// Search types besides the pattern type, from a `type:` filter.
const RESULT_TYPES: &[&str] = &["symbol", "diff", "commit", "repo", "path", "file"];
/// Pattern types.
const PATTERN_TYPES: &[&str] = &["keyword", "standard", "literal", "regexp", "structural"];

/// The type of a search for the "searches by type" breakdown: the result type of a
/// `type:symbol|diff|commit|repo|path|file` filter, else the pattern type (`patterntype:` in
/// the query, else `default`, else `keyword`). `standard` counts as `keyword`.
pub fn search_type(query: &str, default: Option<&str>) -> String {
    let mut result_type = None;
    let mut pattern_type = None;
    for token in query.split_whitespace() {
        let Some((field, value)) = token.split_once(':') else {
            continue;
        };
        let value = value.trim_matches('"').to_ascii_lowercase();
        match field.to_ascii_lowercase().as_str() {
            "type" if RESULT_TYPES.contains(&value.as_str()) && result_type.is_none() => {
                result_type = Some(value);
            }
            "patterntype" if PATTERN_TYPES.contains(&value.as_str()) => {
                pattern_type = Some(value);
            }
            _ => {}
        }
    }
    if let Some(t) = result_type {
        return t;
    }
    let p = pattern_type
        .or_else(|| {
            default
                .map(str::to_ascii_lowercase)
                .filter(|d| PATTERN_TYPES.contains(&d.as_str()))
        })
        .unwrap_or_else(|| "keyword".to_owned());
    if p == "standard" {
        "keyword".to_owned()
    } else {
        p
    }
}

/// Upper bounds (ms) of the search latency buckets; the last one stands for "slower".
pub const LATENCY_BUCKETS_MS: &[i32] = &[
    50,
    100,
    250,
    500,
    1000,
    2500,
    5000,
    10_000,
    30_000,
    i32::MAX,
];

/// The latency bucket (upper bound) of a duration.
pub fn latency_bucket(ms: f64) -> i32 {
    LATENCY_BUCKETS_MS
        .iter()
        .copied()
        .find(|&b| ms <= f64::from(b))
        .unwrap_or(i32::MAX)
}

/// The `q`-quantile (0..=1) of a latency histogram (`(bucket upper bound, count)`, any
/// order): the upper bound of the bucket holding it, the slowest finite bound for the open
/// bucket. `None` without data. Unknown latencies (bucket < 0) are ignored.
pub fn latency_quantile(histogram: &[(i32, i64)], q: f64) -> Option<f64> {
    let mut buckets: Vec<(i32, i64)> = histogram
        .iter()
        .copied()
        .filter(|&(b, n)| b >= 0 && n > 0)
        .collect();
    buckets.sort_unstable();
    let total: i64 = buckets.iter().map(|&(_, n)| n).sum();
    if total == 0 {
        return None;
    }
    let rank = (q.clamp(0.0, 1.0) * total as f64).ceil().max(1.0) as i64;
    let mut seen = 0;
    for (bucket, n) in buckets {
        seen += n;
        if seen >= rank {
            return Some(if bucket == i32::MAX {
                f64::from(LATENCY_BUCKETS_MS[LATENCY_BUCKETS_MS.len() - 2])
            } else {
                f64::from(bucket)
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codeintel_names_round_trip() {
        assert_eq!(
            codeintel_event(CodeIntelAction::Hover, true, false),
            "codeintel.lsifHover"
        );
        assert_eq!(
            codeintel_event(CodeIntelAction::References, false, true),
            "codeintel.searchReferences.xrepo"
        );
        assert_eq!(codeintel_events().len(), 12);
        for n in codeintel_events() {
            assert!(codeintel_action(&n).is_some(), "{n}");
        }
        assert_eq!(codeintel_events_by_precision(true).len(), 6);
        assert!(
            codeintel_cross_repo_events()
                .iter()
                .all(|n| n.ends_with(".xrepo"))
        );
        assert_eq!(
            codeintel_action("codeintel.lsifDefinitions.xrepo"),
            Some(CodeIntelAction::Definitions)
        );
        assert_eq!(codeintel_action("codeintel.other"), None);
        assert_eq!(codeintel_action("ViewBlob"), None);
    }

    #[test]
    fn anonymous_cookie() {
        assert_eq!(
            anonymous_uid_from_cookie("a=b; sourcegullAnonymousUid=3f2a-9c_1; c=d").as_deref(),
            Some("3f2a-9c_1")
        );
        assert_eq!(anonymous_uid_from_cookie("sourcegullAnonymousUid="), None);
        assert_eq!(
            anonymous_uid_from_cookie("sourcegullAnonymousUid=a b"),
            None
        );
        assert_eq!(
            anonymous_uid_from_cookie("sourcegullAnonymousUid=<x>"),
            None
        );
        assert_eq!(anonymous_uid_from_cookie("other=1"), None);
    }

    #[test]
    fn search_types() {
        assert_eq!(search_type("foo", None), "keyword");
        assert_eq!(search_type("foo", Some("standard")), "keyword");
        assert_eq!(search_type("foo", Some("regexp")), "regexp");
        assert_eq!(
            search_type("foo patternType:literal", Some("regexp")),
            "literal"
        );
        assert_eq!(search_type("foo type:symbol", Some("regexp")), "symbol");
        assert_eq!(search_type("repo:x TYPE:Diff author:me", None), "diff");
        assert_eq!(
            search_type("type:bogus x", Some("structural")),
            "structural"
        );
        assert_eq!(search_type("x", Some("nonsense")), "keyword");
    }

    #[test]
    fn latency_buckets_and_quantiles() {
        assert_eq!(latency_bucket(0.0), 50);
        assert_eq!(latency_bucket(50.0), 50);
        assert_eq!(latency_bucket(51.0), 100);
        assert_eq!(latency_bucket(1e9), i32::MAX);
        assert_eq!(latency_quantile(&[], 0.5), None);
        assert_eq!(latency_quantile(&[(-1, 10)], 0.5), None);
        let h = [(100, 50), (50, 40), (1000, 9), (i32::MAX, 1), (-1, 100)];
        assert_eq!(latency_quantile(&h, 0.4), Some(50.0));
        assert_eq!(latency_quantile(&h, 0.5), Some(100.0));
        assert_eq!(latency_quantile(&h, 0.9), Some(100.0));
        assert_eq!(latency_quantile(&h, 0.95), Some(1000.0));
        assert_eq!(latency_quantile(&h, 1.0), Some(30_000.0));
    }
}
