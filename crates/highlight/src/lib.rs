//! sourcegull-highlight: syntax highlighting as compact occurrences.
//!
//! - tree-sitter (`tree-sitter-highlight`) for the compiled-in grammars:
//!   Rust, Go, JavaScript/JSX, TypeScript/TSX, Python, Java, C, C++, JSON,
//!   YAML, Markdown and shell.
//! - syntect (Sublime grammars) as the fallback for other languages, behind
//!   the default `syntect` feature; plain text otherwise.
//! - Output is a list of [`Occurrence`]s (line, UTF-16 columns, kind) for a
//!   CodeMirror viewer, packed with [`pack`] into four `u32`s per range. HTML
//!   ([`to_html`]) is available for API compatibility.
//! - Hard limits ([`Limits`]): files over 1 MB or 20k lines, binary files and
//!   highlights exceeding the timeout come back as plain text.
//!
//! Results depend only on the blob content, path and [`VERSION`], so callers
//! cache them by blob OID + `VERSION`.

mod encode;
mod kind;
#[cfg(feature = "syntect")]
mod syntect_fallback;
mod treesitter;

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use encode::{Occurrence, UnpackError, pack, to_html, unpack};
pub use kind::SyntaxKind;

/// Highlighter version; part of every cache key. Bump when output changes.
pub const VERSION: u32 = 1;

/// Size and time limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Larger files are returned as plain text.
    pub max_bytes: usize,
    /// Files with more lines are returned as plain text.
    pub max_lines: usize,
    /// Highlighting that takes longer is abandoned (plain text).
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024,
            max_lines: 20_000,
            timeout: Duration::from_secs(3),
        }
    }
}

/// Which highlighter produced a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Engine {
    /// tree-sitter.
    TreeSitter,
    /// syntect.
    Syntect,
    /// No highlighting.
    PlainText,
}

/// Why a file was not highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlainReason {
    /// Larger than [`Limits::max_bytes`].
    TooLarge,
    /// More lines than [`Limits::max_lines`].
    TooManyLines,
    /// Highlighting exceeded [`Limits::timeout`].
    Timeout,
    /// The content is binary.
    Binary,
    /// No grammar for the language.
    NoGrammar,
    /// The highlighter reported an error.
    Failed,
}

/// A highlighting result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Highlighted {
    /// The detected language id (see `sourcegull_languages`), if any.
    pub language: Option<String>,
    /// The engine used.
    pub engine: Engine,
    /// Set when `engine` is [`Engine::PlainText`].
    pub plain_reason: Option<PlainReason>,
    /// Ranges, sorted by line then column, non-overlapping.
    pub occurrences: Vec<Occurrence>,
}

impl Highlighted {
    fn plain(language: Option<&str>, reason: PlainReason) -> Self {
        Self {
            language: language.map(str::to_owned),
            engine: Engine::PlainText,
            plain_reason: Some(reason),
            occurrences: Vec::new(),
        }
    }

    /// The occurrences packed with [`pack`].
    pub fn packed(&self) -> Vec<u32> {
        pack(&self.occurrences)
    }
}

/// Internal errors (callers see plain-text results instead).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// No grammar for the language.
    #[error("no grammar for {0}")]
    NoGrammar(String),
    /// The highlighter failed.
    #[error("highlighting failed: {0}")]
    Highlight(String),
    /// The timeout elapsed.
    #[error("highlighting timed out")]
    Timeout,
}

/// A highlight request.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// File path (used for language detection).
    pub path: &'a str,
    /// File content.
    pub content: &'a [u8],
    /// Override detection with this language id or name.
    pub language: Option<&'a str>,
    /// Limits.
    pub limits: Limits,
    /// Set to non-zero from another thread to abort (e.g. client went away).
    pub cancel: Option<&'a AtomicUsize>,
}

impl<'a> Request<'a> {
    /// A request with default limits.
    pub fn new(path: &'a str, content: &'a [u8]) -> Self {
        Self {
            path,
            content,
            language: None,
            limits: Limits::default(),
            cancel: None,
        }
    }
}

/// Language ids highlighted with tree-sitter.
pub fn tree_sitter_languages() -> Vec<&'static str> {
    treesitter::languages()
}

/// Highlights a file. Never fails: problems produce a plain-text result
/// with a [`PlainReason`].
pub fn highlight(req: &Request<'_>) -> Highlighted {
    let content = req.content;
    let head = &content[..content.len().min(16 * 1024)];
    let lang = req
        .language
        .and_then(|l| sourcegull_languages::by_id(l).or_else(|| sourcegull_languages::by_name(l)))
        .or_else(|| sourcegull_languages::detect(req.path, Some(head)));
    let lang_id = lang.map(|l| l.id);
    if content.len() > req.limits.max_bytes {
        return Highlighted::plain(lang_id, PlainReason::TooLarge);
    }
    if head.contains(&0) {
        return Highlighted::plain(lang_id, PlainReason::Binary);
    }
    let lines = memchr_count(content) + 1;
    if lines > req.limits.max_lines {
        return Highlighted::plain(lang_id, PlainReason::TooManyLines);
    }

    if let Some(id) = lang_id.filter(|id| treesitter::supports(id)) {
        return match treesitter::highlight(id, content, &req.limits, req.cancel) {
            Ok(spans) => Highlighted {
                language: Some(id.to_owned()),
                engine: Engine::TreeSitter,
                plain_reason: None,
                occurrences: encode::spans_to_occurrences(content, &spans),
            },
            Err(Error::Timeout) => Highlighted::plain(lang_id, PlainReason::Timeout),
            Err(e) => {
                tracing::warn!(path = req.path, error = %e, "highlighting failed");
                Highlighted::plain(lang_id, PlainReason::Failed)
            }
        };
    }
    #[cfg(feature = "syntect")]
    {
        let text = String::from_utf8_lossy(content);
        let first_line = text.lines().next().unwrap_or_default();
        if let Some(syntax) = syntect_fallback::find(req.path, lang, first_line) {
            return match syntect_fallback::highlight(syntax, &text, &req.limits) {
                Ok(spans) => Highlighted {
                    language: lang_id.map(str::to_owned),
                    engine: Engine::Syntect,
                    plain_reason: None,
                    occurrences: encode::spans_to_occurrences(text.as_bytes(), &spans),
                },
                Err(Error::Timeout) => Highlighted::plain(lang_id, PlainReason::Timeout),
                Err(e) => {
                    tracing::warn!(path = req.path, error = %e, "highlighting failed");
                    Highlighted::plain(lang_id, PlainReason::Failed)
                }
            };
        }
    }
    Highlighted::plain(lang_id, PlainReason::NoGrammar)
}

fn memchr_count(b: &[u8]) -> usize {
    b.iter().filter(|&&c| c == b'\n').count()
}
