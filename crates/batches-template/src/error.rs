//! Template errors, with the position in the template they refer to.

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// The template does not parse.
    Syntax,
    /// A function (or context name such as `repository`) that is not defined.
    UndefinedFunction(String),
    /// A Go template feature this evaluator does not implement (`call`, `template`,
    /// `define`, `block`, complex numbers).
    Unsupported(String),
    /// A field that is not in the map it is looked up in (templates render with
    /// `missingkey=error`). `path` is the expression up to and including the missing key,
    /// e.g. `repository.search_resalt_paths`.
    MissingKey { path: String, key: String },
    /// Any other error while executing (wrong argument types, bad comparisons, …).
    Exec,
    /// The execution or output limits were exceeded.
    Limit,
}

/// An error parsing or executing a template.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{line}:{column}: {message}")]
pub struct TemplateError {
    pub kind: ErrorKind,
    pub message: String,
    /// Byte offset in the template.
    pub offset: usize,
    /// 1-based line.
    pub line: usize,
    /// 1-based column, in characters.
    pub column: usize,
}

impl TemplateError {
    pub(crate) fn new(kind: ErrorKind, src: &str, offset: usize, message: String) -> Self {
        let offset = offset.min(src.len());
        let offset = (0..=offset)
            .rev()
            .find(|&o| src.is_char_boundary(o))
            .unwrap_or(0);
        let before = &src[..offset];
        let line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let column = before[line_start..].chars().count() + 1;
        TemplateError {
            kind,
            message,
            offset,
            line,
            column,
        }
    }
}

/// An error before it is placed in the source.
#[derive(Debug, Clone)]
pub(crate) struct RawError {
    pub kind: ErrorKind,
    pub pos: usize,
    pub message: String,
}

impl RawError {
    pub(crate) fn syntax(pos: usize, message: impl Into<String>) -> Self {
        RawError {
            kind: ErrorKind::Syntax,
            pos,
            message: message.into(),
        }
    }

    pub(crate) fn exec(pos: usize, message: impl Into<String>) -> Self {
        RawError {
            kind: ErrorKind::Exec,
            pos,
            message: message.into(),
        }
    }

    pub(crate) fn place(self, src: &str) -> TemplateError {
        TemplateError::new(self.kind, src, self.pos, self.message)
    }
}
