//! Blame, streamed.
//!
//! Two backends:
//! - [`blame_stream`] runs `git blame --incremental` and streams hunks as git
//!   finds them, so the first hunks reach the UI long before the oldest lines
//!   are attributed. It uses the commit-graph's changed-path Bloom filters.
//! - [`blame_gix`] runs gitoxide's blame in-process and returns all hunks at once.
//!
//! The service uses the CLI backend. Measured on ripgrep (2.3k commits,
//! commit-graph with Bloom filters, release build, `tests/blame_bench.rs`):
//! the CLI took 12–47 ms in total against 18–98 ms for gix (1.2–2.7× faster),
//! and its first hunk arrived after 4–6 ms. gix blame stays for comparison as
//! gitoxide improves.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use gix::ObjectId;
use gix::bstr::ByteSlice;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::command::GitCommand;
use crate::error::{GitError, Result};
use crate::read::{self, Signature, normalize_path};

/// Lines of one file attributed to one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameHunk {
    /// The commit that introduced the lines.
    pub commit: ObjectId,
    /// 1-based first line in the blamed file.
    pub start_line: u32,
    /// Number of lines.
    pub line_count: u32,
    /// 1-based first line in `commit`'s version of the file.
    pub orig_start_line: u32,
    /// The file's path in `commit` (differs after renames).
    pub orig_path: String,
    /// Author of `commit`.
    pub author: Signature,
    /// First line of `commit`'s message.
    pub summary: String,
}

/// Options for blame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameOptions {
    /// 1-based first line; 0 for the start of the file.
    pub start_line: u32,
    /// 1-based last line (inclusive); 0 for the end of the file.
    pub end_line: u32,
    /// Ignore whitespace changes (`-w`).
    pub ignore_whitespace: bool,
    /// Kill the git process after this long.
    pub timeout: Duration,
}

impl Default for BlameOptions {
    fn default() -> Self {
        Self {
            start_line: 0,
            end_line: 0,
            ignore_whitespace: false,
            timeout: Duration::from_secs(60),
        }
    }
}

/// Streams blame hunks for `path` at `commit` from `git blame --incremental`.
///
/// Hunks arrive in the order git attributes them, not in line order. The
/// stream ends with an error item if git fails. Dropping the stream kills git.
pub fn blame_stream(
    git_bin: &Path,
    repo_dir: &Path,
    commit: ObjectId,
    path: &str,
    opts: &BlameOptions,
) -> Result<ReceiverStream<Result<BlameHunk>>> {
    let path = normalize_path(path)?;
    if path.is_empty() {
        return Err(GitError::InvalidArgument("blame needs a file path".into()));
    }
    let mut cmd = GitCommand::new(git_bin, "blame")
        .repo(repo_dir)
        .arg("--incremental")
        .timeout(opts.timeout);
    if opts.ignore_whitespace {
        cmd = cmd.arg("-w");
    }
    if opts.start_line > 0 || opts.end_line > 0 {
        let start = opts.start_line.max(1);
        let range = if opts.end_line > 0 {
            format!("{start},{}", opts.end_line)
        } else {
            format!("{start},")
        };
        cmd = cmd.arg("-L").arg(range);
    }
    let mut running = cmd
        .arg(commit.to_string())
        .arg("--")
        .arg(&path)
        .spawn_streaming()?;
    let stdout = running.take_stdout().expect("stdout is piped");
    let deadline = running.deadline();
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut parser = IncrementalParser::default();
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let line = tokio::select! {
                l = lines.next_line() => l,
                _ = tokio::time::sleep_until(deadline) => break,
            };
            match line {
                Ok(Some(line)) => {
                    if let Some(hunk) = parser.push(&line) {
                        if tx.send(Ok(hunk)).await.is_err() {
                            return; // receiver gone; dropping `running` kills git
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(GitError::Io(e))).await;
                    return;
                }
            }
        }
        if let Err(e) = running.finish().await {
            let e = match e {
                GitError::Command { stderr, .. }
                    if stderr.contains("no such path") || stderr.contains("no such ref") =>
                {
                    GitError::PathNotFound(path)
                }
                e => e,
            };
            let _ = tx.send(Err(e)).await;
        }
    });
    Ok(ReceiverStream::new(rx))
}

/// Parses `git blame --incremental` output (see git-blame(1), "THE PORCELAIN
/// FORMAT"). Commit headers appear only the first time a commit is seen; each
/// entry ends with a `filename` line.
#[derive(Default)]
struct IncrementalParser {
    current: Option<Entry>,
    commits: HashMap<ObjectId, (Signature, String)>,
}

struct Entry {
    commit: ObjectId,
    orig_line: u32,
    final_line: u32,
    count: u32,
    author: Signature,
    summary: Option<String>,
}

impl IncrementalParser {
    fn push(&mut self, line: &str) -> Option<BlameHunk> {
        if self.current.is_none() {
            let mut parts = line.split(' ');
            let commit = ObjectId::from_hex(parts.next()?.as_bytes()).ok()?;
            let orig_line = parts.next()?.parse().ok()?;
            let final_line = parts.next()?.parse().ok()?;
            let count = parts.next()?.parse().ok()?;
            self.current = Some(Entry {
                commit,
                orig_line,
                final_line,
                count,
                author: Signature {
                    name: String::new(),
                    email: String::new(),
                    time: 0,
                    tz_offset_seconds: 0,
                },
                summary: None,
            });
            return None;
        }
        let entry = self.current.as_mut().expect("checked above");
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "author" => entry.author.name = value.to_owned(),
            "author-mail" => {
                entry.author.email = value
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            }
            "author-time" => entry.author.time = value.parse().unwrap_or_default(),
            "author-tz" => entry.author.tz_offset_seconds = parse_tz(value),
            "summary" => entry.summary = Some(value.to_owned()),
            "filename" => {
                let entry = self.current.take().expect("checked above");
                let (author, summary) = match entry.summary {
                    Some(summary) => {
                        self.commits
                            .insert(entry.commit, (entry.author.clone(), summary.clone()));
                        (entry.author, summary)
                    }
                    None => self
                        .commits
                        .get(&entry.commit)
                        .cloned()
                        .unwrap_or((entry.author, String::new())),
                };
                return Some(BlameHunk {
                    commit: entry.commit,
                    start_line: entry.final_line,
                    line_count: entry.count,
                    orig_start_line: entry.orig_line,
                    orig_path: unquote(value),
                    author,
                    summary,
                });
            }
            _ => {} // committer*, previous, boundary
        }
        None
    }
}

/// `+0130` → 5400 seconds.
fn parse_tz(tz: &str) -> i32 {
    let (sign, digits) = match tz.as_bytes().first() {
        Some(b'-') => (-1, &tz[1..]),
        Some(b'+') => (1, &tz[1..]),
        _ => (1, tz),
    };
    let n: i32 = digits.parse().unwrap_or_default();
    sign * ((n / 100) * 3600 + (n % 100) * 60)
}

/// Undoes git's C-style quoting of unusual file names (`"a\tb"`).
fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_owned();
    };
    let mut out = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1;
            match bytes[i] {
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'"' => out.push(b'"'),
                b'\\' => out.push(b'\\'),
                b'0'..=b'7' if i + 2 < bytes.len() => {
                    let oct = &inner[i..i + 3];
                    out.push(u8::from_str_radix(oct, 8).unwrap_or(b'?'));
                    i += 2;
                }
                c => out.push(c),
            }
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Blame through gitoxide, in-process, returning hunks in line order.
pub fn blame_gix(
    repo: &gix::Repository,
    commit: ObjectId,
    path: &str,
    opts: &BlameOptions,
) -> Result<Vec<BlameHunk>> {
    let path = normalize_path(path)?;
    read::stat(repo, commit, &path)?;
    let ranges = if opts.start_line > 0 || opts.end_line > 0 {
        let end = if opts.end_line > 0 {
            opts.end_line
        } else {
            u32::MAX
        };
        gix::blame::BlameRanges::from_one_based_inclusive_range(opts.start_line.max(1)..=end)
            .map_err(|e| GitError::InvalidArgument(e.to_string()))?
    } else {
        gix::blame::BlameRanges::default()
    };
    let outcome = repo.blame_file(
        path.as_bytes().as_bstr(),
        commit,
        gix::repository::blame_file::Options {
            ranges,
            rewrites: Some(gix::diff::Rewrites::default()),
            ..Default::default()
        },
    )?;
    let mut commits: HashMap<ObjectId, (Signature, String)> = HashMap::new();
    let mut out = Vec::with_capacity(outcome.entries.len());
    for e in outcome.entries {
        let (author, summary) = match commits.get(&e.commit_id) {
            Some(v) => v.clone(),
            None => {
                let info = read::commit_info(repo, e.commit_id)?;
                let v = (info.author.clone(), info.subject().to_owned());
                commits.insert(e.commit_id, v.clone());
                v
            }
        };
        out.push(BlameHunk {
            commit: e.commit_id,
            start_line: e.start_in_blamed_file + 1,
            line_count: e.len.get(),
            orig_start_line: e.start_in_source_file + 1,
            orig_path: e
                .source_file_name
                .map(|p| p.to_str_lossy().into_owned())
                .unwrap_or_else(|| path.clone()),
            author,
            summary,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_incremental_output() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let out = format!(
            "{a} 1 1 2\nauthor Ann\nauthor-mail <ann@x>\nauthor-time 100\nauthor-tz -0130\n\
             committer Ann\nsummary first\nboundary\nfilename src/a.rs\n\
             {b} 3 3 1\nauthor Bob\nauthor-mail <bob@x>\nauthor-time 200\nauthor-tz +0000\n\
             summary second\nprevious {a} src/a.rs\nfilename \"src/\\303\\244.rs\"\n\
             {a} 5 4 1\nfilename src/a.rs\n"
        );
        let mut p = IncrementalParser::default();
        let hunks: Vec<BlameHunk> = out.lines().filter_map(|l| p.push(l)).collect();
        assert_eq!(hunks.len(), 3);
        assert_eq!(hunks[0].author.name, "Ann");
        assert_eq!(hunks[0].author.email, "ann@x");
        assert_eq!(hunks[0].author.tz_offset_seconds, -5400);
        assert_eq!((hunks[0].start_line, hunks[0].line_count), (1, 2));
        assert_eq!(hunks[1].orig_path, "src/ä.rs");
        assert_eq!(hunks[1].summary, "second");
        // Repeated commit reuses the cached header.
        assert_eq!(hunks[2].summary, "first");
        assert_eq!(hunks[2].author.time, 100);
        assert_eq!((hunks[2].start_line, hunks[2].orig_start_line), (4, 5));
    }
}
