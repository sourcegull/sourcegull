//! What actions send: the email (HTML and text), the generic webhook's JSON payload, the
//! Slack-compatible incoming webhook payload (Block Kit sections, which Rocket.Chat accepts
//! too, with a plain `text` fallback), the Mattermost incoming webhook payload (Markdown) and
//! the Telegram Bot API `sendMessage` request (HTML).
//!
//! Without `includeResults` a notification names the watch, its query and the number of
//! new results, with links; with it, the results follow (commit subject, author and link, and
//! for diff searches the matching hunks), at most [`MAX_RENDERED`] in emails and chat
//! messages, and in Telegram messages only as many as fit in [`TELEGRAM_MAX_LEN`]. Webhook
//! payloads carry every stored result.

use serde_json::{Value, json};
use sourcegull_services::relay::marshal_id;

use crate::results::MonitorResult;
use crate::store::Priority;

/// Node kind of watch IDs.
pub const MONITOR_KIND: &str = "CodeMonitor";

/// Results shown in an email or chat message.
pub const MAX_RENDERED: usize = 10;

/// Diff lines shown per result in an email or chat message.
const MAX_DIFF_LINES: usize = 20;

/// The longest Telegram message, in UTF-16 code units.
pub const TELEGRAM_MAX_LEN: usize = 4096;

/// Characters of the query shown in a Telegram message (a query can be 16 KiB).
const TELEGRAM_MAX_QUERY: usize = 500;

/// Whose access chose the results of an email.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience<'a> {
    /// The recipient's own: the results are limited to repositories they can read.
    Recipient,
    /// An unfiltered watch: every recipient gets the results `shared_by` can read.
    Everyone { shared_by: &'a str },
    /// The watch's owner alone, or a test message: no note.
    Owner,
}

impl Audience<'_> {
    /// The note an email ends with about which results it holds.
    pub fn note(&self) -> Option<String> {
        match self {
            Audience::Recipient => Some(
                "Results are limited to repositories you can read. Other recipients may get \
                 different results."
                    .to_owned(),
            ),
            Audience::Everyone { shared_by } => Some(format!(
                "Unfiltered: every recipient gets all results this watch finds with \
                 {shared_by}'s access, including results in repositories they cannot read."
            )),
            Audience::Owner => None,
        }
    }
}

/// What happened, for one action.
#[derive(Debug, Clone)]
pub struct Notification<'a> {
    pub monitor_id: i64,
    pub description: &'a str,
    pub query: &'a str,
    pub results: &'a [MonitorResult],
    /// `externalURL` of the site, for absolute links.
    pub external_url: &'a str,
    /// A "send test" message.
    pub test: bool,
}

impl Notification<'_> {
    /// The watch's page.
    pub fn monitor_url(&self) -> String {
        format!(
            "{}/watches/{}",
            self.base(),
            marshal_id(MONITOR_KIND, self.monitor_id)
        )
    }

    /// A search for the watch's query.
    pub fn search_url(&self) -> String {
        let q: String = url::form_urlencoded::byte_serialize(self.query.as_bytes()).collect();
        format!("{}/search?q={q}", self.base())
    }

    fn base(&self) -> &str {
        self.external_url.trim_end_matches('/')
    }

    fn absolute(&self, url: &str) -> String {
        if url.starts_with('/') {
            format!("{}{url}", self.base())
        } else {
            url.to_owned()
        }
    }

    fn summary(&self) -> String {
        if self.test {
            return format!(
                "Test message for watch \"{}\": this is how its notifications will look.",
                self.description
            );
        }
        let n = self.results.len();
        format!(
            "Watch \"{}\" found {n} new {}.",
            self.description,
            if n == 1 { "result" } else { "results" }
        )
    }

    fn more(&self) -> Option<String> {
        let rest = self.results.len().saturating_sub(MAX_RENDERED);
        (rest > 0).then(|| format!("…and {rest} more"))
    }
}

/// The first `MAX_DIFF_LINES` lines of a result's diff, with a note when cut.
fn diff_excerpt(diff: &str) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    let mut out = lines
        .iter()
        .take(MAX_DIFF_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > MAX_DIFF_LINES {
        out.push_str(&format!("\n… {} more lines", lines.len() - MAX_DIFF_LINES));
    }
    out
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A rendered email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    pub subject: String,
    pub text: String,
    pub html: String,
}

/// The email of an email action.
pub fn email(
    n: &Notification<'_>,
    priority: Priority,
    include_results: bool,
    audience: Audience<'_>,
) -> Email {
    let count = n.results.len();
    let mut subject = if n.test {
        format!("Test: watch \"{}\"", n.description)
    } else {
        format!(
            "Watch \"{}\": {count} new {}",
            n.description,
            if count == 1 { "result" } else { "results" }
        )
    };
    if priority == Priority::Critical {
        subject = format!("[CRITICAL] {subject}");
    }
    let monitor_url = n.monitor_url();
    let search_url = n.search_url();

    let mut text = format!("{}\n\nQuery: {}\n", n.summary(), n.query);
    let mut html = format!(
        "<div style=\"font-family:sans-serif\"><p>{}</p><p>Query: <code>{}</code></p>",
        html_escape(&n.summary()),
        html_escape(n.query)
    );
    if include_results && !n.results.is_empty() {
        text.push('\n');
        html.push_str("<ul style=\"padding-left:1em\">");
        for r in n.results.iter().take(MAX_RENDERED) {
            let url = n.absolute(&r.url);
            text.push_str(&format!(
                "- {} › {} ({}) by {}\n  {url}\n",
                r.repository,
                r.subject,
                r.short_commit(),
                r.author_name
            ));
            html.push_str(&format!(
                "<li style=\"margin-bottom:1em\"><a href=\"{}\">{} › {}</a> \
                 (<code>{}</code>) by {}",
                html_escape(&url),
                html_escape(&r.repository),
                html_escape(&r.subject),
                html_escape(r.short_commit()),
                html_escape(&r.author_name)
            ));
            if !r.diff.is_empty() {
                let excerpt = diff_excerpt(&r.diff);
                for line in excerpt.lines() {
                    text.push_str(&format!("    {line}\n"));
                }
                html.push_str(&format!(
                    "<pre style=\"background:#f6f8fa;padding:0.5em;overflow:auto\">{}</pre>",
                    html_escape(&excerpt)
                ));
            }
            html.push_str("</li>");
        }
        html.push_str("</ul>");
        if let Some(more) = n.more() {
            text.push_str(&format!("{more}\n"));
            html.push_str(&format!("<p>{}</p>", html_escape(&more)));
        }
    }
    if let Some(note) = audience.note() {
        text.push_str(&format!("\n{note}\n"));
        let color = match audience {
            Audience::Everyone { .. } => "#b91c1c",
            _ => "#57606a",
        };
        html.push_str(&format!(
            "<p style=\"color:{color};font-size:0.9em\">{}</p>",
            html_escape(&note)
        ));
    }
    text.push_str(&format!(
        "\nSee the results: {search_url}\nManage the watch: {monitor_url}\n"
    ));
    html.push_str(&format!(
        "<p><a href=\"{}\">See the results</a> · <a href=\"{}\">Manage the watch</a></p></div>",
        html_escape(&search_url),
        html_escape(&monitor_url)
    ));
    Email {
        subject,
        text,
        html,
    }
}

/// The JSON body of a generic webhook action.
pub fn webhook_payload(n: &Notification<'_>, include_results: bool) -> Value {
    let mut payload = json!({
        "monitorDescription": n.description,
        "monitorURL": n.monitor_url(),
        "query": n.query,
        "searchURL": n.search_url(),
        "resultCount": n.results.len(),
        "test": n.test,
    });
    if include_results {
        let results: Vec<Value> = n
            .results
            .iter()
            .map(|r| {
                let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
                v["url"] = Value::String(n.absolute(&r.url));
                v
            })
            .collect();
        payload["results"] = Value::Array(results);
    }
    payload
}

/// Escapes `&`, `<` and `>` for Slack's mrkdwn.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The JSON body of a Slack (or compatible) incoming webhook action.
pub fn slack_payload(n: &Notification<'_>, include_results: bool) -> Value {
    let mut blocks = vec![json!({
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": format!(
                "{}\n*<{}|{}>* · `{}`",
                slack_escape(&n.summary()),
                n.monitor_url(),
                slack_escape(n.description),
                slack_escape(n.query)
            ),
        },
    })];
    if include_results && !n.results.is_empty() {
        blocks.push(json!({"type": "divider"}));
        for r in n.results.iter().take(MAX_RENDERED) {
            let mut text = format!(
                "<{}|{} › {}> (`{}`) by {}",
                n.absolute(&r.url),
                slack_escape(&r.repository),
                slack_escape(&r.subject),
                r.short_commit(),
                slack_escape(&r.author_name)
            );
            if !r.diff.is_empty() {
                text.push_str(&format!("\n```{}```", slack_escape(&diff_excerpt(&r.diff))));
            }
            blocks.push(json!({
                "type": "section",
                "text": {"type": "mrkdwn", "text": text},
            }));
        }
        if let Some(more) = n.more() {
            blocks.push(json!({
                "type": "context",
                "elements": [{"type": "mrkdwn", "text": more}],
            }));
        }
    }
    blocks.push(json!({
        "type": "actions",
        "elements": [{
            "type": "button",
            "text": {"type": "plain_text", "text": "See the results"},
            "url": n.search_url(),
        }],
    }));
    json!({ "text": n.summary(), "blocks": blocks })
}

/// Backslash-escapes Markdown punctuation for Mattermost.
fn md_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '<' | '>' | '#' | '~' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The longest run of backticks in `s`.
fn longest_backtick_run(s: &str) -> usize {
    s.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// `s` as a Markdown code span, fenced with more backticks than it contains.
fn md_code(s: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(s) + 1);
    format!("{fence} {s} {fence}")
}

/// `s` as a Markdown code block, fenced with more backticks than it contains.
fn md_code_block(s: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(s).max(2) + 1);
    format!("{fence}\n{s}\n{fence}")
}

/// The JSON body of a Mattermost incoming webhook action: one Markdown message.
pub fn mattermost_payload(n: &Notification<'_>, include_results: bool) -> Value {
    let mut text = format!(
        "{}\n[{}]({})",
        md_escape(&n.summary()),
        md_escape(n.description),
        n.monitor_url()
    );
    if !n.query.is_empty() {
        text.push_str(&format!(" · {}", md_code(n.query)));
    }
    if include_results && !n.results.is_empty() {
        text.push('\n');
        for r in n.results.iter().take(MAX_RENDERED) {
            text.push_str(&format!(
                "\n- [{} › {}]({}) ({}) by {}",
                md_escape(&r.repository),
                md_escape(&r.subject),
                n.absolute(&r.url),
                md_code(r.short_commit()),
                md_escape(&r.author_name)
            ));
            if !r.diff.is_empty() {
                let block = md_code_block(&diff_excerpt(&r.diff));
                for line in block.lines() {
                    text.push_str(&format!("\n  {line}"));
                }
            }
        }
        if let Some(more) = n.more() {
            text.push_str(&format!("\n\n{more}"));
        }
    }
    text.push_str(&format!("\n\n[See the results]({})", n.search_url()));
    json!({ "text": text })
}

/// The first `max` characters of `s`, with `…` when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The text of a Telegram message (`parse_mode: HTML`). Results are added while the message
/// stays within [`TELEGRAM_MAX_LEN`] (measured on the HTML, which is never shorter than the
/// text Telegram counts).
pub fn telegram_text(n: &Notification<'_>, include_results: bool) -> String {
    let mut head = format!("<b>{}</b>", html_escape(&n.summary()));
    if !n.query.is_empty() {
        head.push_str(&format!(
            "\nQuery: <code>{}</code>",
            html_escape(&truncate_chars(n.query, TELEGRAM_MAX_QUERY))
        ));
    }
    // The search link holds the whole query; a long one would crowd out the results.
    let search_url = n.search_url();
    let mut foot = String::from("\n\n");
    if search_url.len() <= TELEGRAM_MAX_QUERY * 2 {
        foot.push_str(&format!(
            "<a href=\"{}\">See the results</a> · ",
            html_escape(&search_url)
        ));
    }
    foot.push_str(&format!(
        "<a href=\"{}\">Manage the watch</a>",
        html_escape(&n.monitor_url())
    ));
    let mut body = String::new();
    if include_results && !n.results.is_empty() {
        // Room for the "…and N more" line.
        let reserve = 40;
        let budget = TELEGRAM_MAX_LEN.saturating_sub(utf16_len(&head) + utf16_len(&foot) + reserve);
        let mut shown = 0;
        for r in n.results.iter().take(MAX_RENDERED) {
            let mut item = format!(
                "\n\n• <a href=\"{}\">{} › {}</a> (<code>{}</code>) by {}",
                html_escape(&n.absolute(&r.url)),
                html_escape(&r.repository),
                html_escape(&r.subject),
                html_escape(r.short_commit()),
                html_escape(&r.author_name)
            );
            if !r.diff.is_empty() {
                item.push_str(&format!(
                    "\n<pre>{}</pre>",
                    html_escape(&diff_excerpt(&r.diff))
                ));
            }
            if utf16_len(&body) + utf16_len(&item) > budget {
                break;
            }
            body.push_str(&item);
            shown += 1;
        }
        let rest = n.results.len() - shown;
        if rest > 0 {
            body.push_str(&format!("\n\n…and {rest} more"));
        }
    }
    format!("{head}{body}{foot}")
}

/// The JSON body of a Telegram action's `sendMessage` request to `chat_id`.
pub fn telegram_payload(n: &Notification<'_>, chat_id: &str, include_results: bool) -> Value {
    json!({
        "chat_id": chat_id,
        "text": telegram_text(n, include_results),
        "parse_mode": "HTML",
        "link_preview_options": {"is_disabled": true},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(i: usize, diff: &str) -> MonitorResult {
        MonitorResult {
            kind: if diff.is_empty() { "commit" } else { "diff" }.into(),
            repository: "gitlab.example.com/acme/web".into(),
            repository_id: 1,
            commit: format!("{i:07}abcdef"),
            url: format!("/gitlab.example.com/acme/web/-/commit/{i:07}abcdef"),
            subject: format!("change <{i}> & more"),
            message: format!("change <{i}> & more\n"),
            author_name: "Alice".into(),
            author_date: "2026-10-01T00:00:00Z".into(),
            diff: diff.into(),
        }
    }

    fn notification(results: &[MonitorResult]) -> Notification<'_> {
        Notification {
            monitor_id: 7,
            description: "TODOs <added>",
            query: "type:diff TODO",
            results,
            external_url: "https://sourcegull.example.com/",
            test: false,
        }
    }

    #[test]
    fn email_with_and_without_results() {
        let results: Vec<MonitorResult> = (0..12)
            .map(|i| result(i, "a.rs a.rs\n@@ -1,0 +1,1 @@\n+// TODO <x>"))
            .collect();
        let n = notification(&results);
        let e = email(&n, Priority::Normal, true, Audience::Recipient);
        assert_eq!(e.subject, "Watch \"TODOs <added>\": 12 new results");
        assert!(
            e.text
                .contains("\nResults are limited to repositories you can read.")
        );
        assert!(
            e.text
                .starts_with("Watch \"TODOs <added>\" found 12 new results.\n")
        );
        assert!(e.text.contains("Query: type:diff TODO\n"));
        assert!(e.text.contains(
            "- gitlab.example.com/acme/web › change <0> & more (0000000) by Alice\n  \
             https://sourcegull.example.com/gitlab.example.com/acme/web/-/commit/0000000abcdef\n"
        ));
        assert!(e.text.contains("    +// TODO <x>\n"));
        assert!(e.text.contains("…and 2 more\n"));
        assert!(e.text.contains(
            "See the results: https://sourcegull.example.com/search?q=type%3Adiff+TODO\n"
        ));
        let monitor = format!(
            "https://sourcegull.example.com/watches/{}",
            marshal_id(MONITOR_KIND, 7)
        );
        assert!(e.text.contains(&format!("Manage the watch: {monitor}\n")));
        // HTML is escaped; ten results are rendered.
        assert!(e.html.contains("TODOs &lt;added&gt;"));
        assert!(e.html.contains("change &lt;9&gt; &amp; more"));
        assert!(!e.html.contains("change &lt;10&gt;"));
        assert!(e.html.contains("<pre style=\"background:#f6f8fa;padding:0.5em;overflow:auto\">a.rs a.rs\n@@ -1,0 +1,1 @@\n+// TODO &lt;x&gt;</pre>"));
        assert!(!e.html.contains("<x>"));
        assert_eq!(e.html.matches("<li").count(), MAX_RENDERED);

        let e = email(
            &notification(&results[..1]),
            Priority::Critical,
            false,
            Audience::Everyone { shared_by: "alice" },
        );
        assert!(
            e.text
                .contains("all results this watch finds with alice's access")
        );
        assert!(e.html.contains("color:#b91c1c"));
        assert_eq!(
            e.subject,
            "[CRITICAL] Watch \"TODOs <added>\": 1 new result"
        );
        assert!(!e.text.contains("acme/web"));
        assert!(!e.html.contains("<li"));

        let test = Notification {
            test: true,
            ..notification(&[])
        };
        let e = email(&test, Priority::Normal, true, Audience::Owner);
        assert!(!e.text.contains("Results are limited"));
        assert_eq!(e.subject, "Test: watch \"TODOs <added>\"");
        assert!(e.text.starts_with("Test message for watch"));
    }

    #[test]
    fn long_diffs_are_cut() {
        let diff: String = (0..30).map(|i| format!("+line {i}\n")).collect();
        let x = diff_excerpt(&diff);
        assert_eq!(x.lines().count(), MAX_DIFF_LINES + 1);
        assert!(x.ends_with("… 10 more lines"));
    }

    #[test]
    fn webhook_and_slack_payloads() {
        let results = vec![result(1, ""), result(2, "b.go b.go\n@@ -1 +1 @@\n-x\n+y")];
        let n = notification(&results);
        let p = webhook_payload(&n, false);
        assert_eq!(p["monitorDescription"], "TODOs <added>");
        assert_eq!(p["query"], "type:diff TODO");
        assert_eq!(p["resultCount"], 2);
        assert_eq!(p["test"], false);
        assert!(p.get("results").is_none());
        let p = webhook_payload(&n, true);
        assert_eq!(p["results"].as_array().unwrap().len(), 2);
        assert_eq!(
            p["results"][0]["url"],
            "https://sourcegull.example.com/gitlab.example.com/acme/web/-/commit/0000001abcdef"
        );
        assert_eq!(p["results"][1]["diff"], "b.go b.go\n@@ -1 +1 @@\n-x\n+y");

        let s = slack_payload(&n, true);
        assert_eq!(s["text"], "Watch \"TODOs <added>\" found 2 new results.");
        let blocks = s["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 5);
        let head = blocks[0]["text"]["text"].as_str().unwrap();
        assert!(head.contains("TODOs &lt;added&gt;"), "{head}");
        let second = blocks[3]["text"]["text"].as_str().unwrap();
        assert!(second.contains("change &lt;2&gt; &amp; more"));
        assert!(second.ends_with("```b.go b.go\n@@ -1 +1 @@\n-x\n+y```"));
        assert_eq!(blocks[4]["type"], "actions");
        assert_eq!(
            slack_payload(&n, false)["blocks"].as_array().unwrap().len(),
            2
        );
    }

    #[test]
    fn mattermost_payload_is_escaped_markdown() {
        let results = vec![
            result(1, ""),
            result(2, "b.go b.go\n@@ -1 +1 @@\n-x\n+y = `z` ```"),
        ];
        let n = notification(&results);
        let text = mattermost_payload(&n, true)["text"]
            .as_str()
            .unwrap()
            .to_owned();
        let monitor = format!(
            "https://sourcegull.example.com/watches/{}",
            marshal_id(MONITOR_KIND, 7)
        );
        assert!(
            text.starts_with(&format!(
                "Watch \"TODOs \\<added\\>\" found 2 new results.\n\
                 [TODOs \\<added\\>]({monitor}) · ` type:diff TODO `\n"
            )),
            "{text}"
        );
        assert!(text.contains(
            "\n- [gitlab.example.com/acme/web › change \\<2\\> & more]\
             (https://sourcegull.example.com/gitlab.example.com/acme/web/-/commit/0000002abcdef) \
             (` 0000002 `) by Alice\n  ````\n  b.go b.go\n"
        ));
        assert!(text.contains("+y = `z` ```\n  ````"), "{text}");
        assert!(text.ends_with(
            "\n\n[See the results](https://sourcegull.example.com/search?q=type%3Adiff+TODO)"
        ));
        let short = mattermost_payload(&n, false)["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!short.contains("acme/web"));
    }

    #[test]
    fn telegram_payload_is_html_within_the_limit() {
        let results = vec![result(1, ""), result(2, "b.go b.go\n@@ -1 +1 @@\n-x\n+y")];
        let n = notification(&results);
        let p = telegram_payload(&n, "-1001234", true);
        assert_eq!(p["chat_id"], "-1001234");
        assert_eq!(p["parse_mode"], "HTML");
        let text = p["text"].as_str().unwrap();
        assert!(text.starts_with(
            "<b>Watch &quot;TODOs &lt;added&gt;&quot; found 2 new results.</b>\n\
             Query: <code>type:diff TODO</code>\n\n• <a href=\""
        ));
        assert!(text.contains("change &lt;2&gt; &amp; more</a> (<code>0000002</code>) by Alice"));
        assert!(text.contains("<pre>b.go b.go\n@@ -1 +1 @@\n-x\n+y</pre>"));
        assert!(text.ends_with(">Manage the watch</a>"));
        assert!(!text.contains("more\n\n…and"));

        // Results that do not fit are counted instead.
        let big: String = (0..20)
            .map(|i| format!("+{}\n", "x".repeat(100 + i)))
            .collect();
        let results: Vec<MonitorResult> = (0..12).map(|i| result(i, &big)).collect();
        let n = notification(&results);
        let text = telegram_text(&n, true);
        assert!(utf16_len(&text) <= TELEGRAM_MAX_LEN, "{}", utf16_len(&text));
        let shown = text.matches("<pre>").count();
        assert!((1..MAX_RENDERED).contains(&shown), "{shown}");
        assert!(text.contains(&format!("…and {} more", 12 - shown)));

        // A long query is cut.
        let query = "type:diff ".repeat(1000);
        let n = Notification {
            query: &query,
            ..notification(&[])
        };
        let text = telegram_text(&n, true);
        assert!(utf16_len(&text) < 1000, "{text}");
        assert!(!text.contains("See the results"));
    }
}
