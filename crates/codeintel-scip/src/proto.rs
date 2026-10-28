//! The SCIP messages processing reads, written by hand with `prost` derives.
//!
//! Field numbers follow the SCIP protocol definition (`scip.proto`). Fields that
//! processing does not keep are left out; prost skips unknown fields. The top-level `Index`
//! message is never decoded whole: [`crate::stream`] walks its fields one at a time.

#![allow(clippy::doc_markdown)]

/// `Index` field numbers.
pub const INDEX_METADATA: u32 = 1;
pub const INDEX_DOCUMENTS: u32 = 2;
pub const INDEX_EXTERNAL_SYMBOLS: u32 = 3;

#[derive(Clone, PartialEq, prost::Message)]
pub struct Metadata {
    #[prost(int32, tag = "1")]
    pub version: i32,
    #[prost(message, optional, tag = "2")]
    pub tool_info: Option<ToolInfo>,
    #[prost(string, tag = "3")]
    pub project_root: String,
    #[prost(int32, tag = "4")]
    pub text_document_encoding: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ToolInfo {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub version: String,
    #[prost(string, repeated, tag = "3")]
    pub arguments: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Document {
    #[prost(string, tag = "4")]
    pub language: String,
    #[prost(string, tag = "1")]
    pub relative_path: String,
    #[prost(message, repeated, tag = "2")]
    pub occurrences: Vec<Occurrence>,
    #[prost(message, repeated, tag = "3")]
    pub symbols: Vec<SymbolInformation>,
    // `text` (5) is skipped: file contents come from git.
    #[prost(int32, tag = "6")]
    pub position_encoding: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SymbolInformation {
    #[prost(string, tag = "1")]
    pub symbol: String,
    #[prost(string, repeated, tag = "3")]
    pub documentation: Vec<String>,
    #[prost(message, repeated, tag = "4")]
    pub relationships: Vec<Relationship>,
    #[prost(int32, tag = "5")]
    pub kind: i32,
    #[prost(string, tag = "6")]
    pub display_name: String,
    #[prost(message, optional, tag = "7")]
    pub signature_documentation: Option<Signature>,
    #[prost(string, tag = "8")]
    pub enclosing_symbol: String,
}

/// `Document` as used for `signature_documentation`; only its text is kept.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Signature {
    #[prost(string, tag = "4")]
    pub language: String,
    #[prost(string, tag = "5")]
    pub text: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Relationship {
    #[prost(string, tag = "1")]
    pub symbol: String,
    #[prost(bool, tag = "2")]
    pub is_reference: bool,
    #[prost(bool, tag = "3")]
    pub is_implementation: bool,
    #[prost(bool, tag = "4")]
    pub is_type_definition: bool,
    #[prost(bool, tag = "5")]
    pub is_definition: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SingleLineRange {
    #[prost(int32, tag = "1")]
    pub line: i32,
    #[prost(int32, tag = "2")]
    pub start_character: i32,
    #[prost(int32, tag = "3")]
    pub end_character: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MultiLineRange {
    #[prost(int32, tag = "1")]
    pub start_line: i32,
    #[prost(int32, tag = "2")]
    pub start_character: i32,
    #[prost(int32, tag = "3")]
    pub end_line: i32,
    #[prost(int32, tag = "4")]
    pub end_character: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Occurrence {
    /// Deprecated packed form: `[line, start, end]` or `[startLine, start, endLine, end]`.
    #[prost(int32, repeated, tag = "1")]
    pub range: Vec<i32>,
    #[prost(message, optional, tag = "8")]
    pub single_line_range: Option<SingleLineRange>,
    #[prost(message, optional, tag = "9")]
    pub multi_line_range: Option<MultiLineRange>,
    #[prost(string, tag = "2")]
    pub symbol: String,
    #[prost(int32, tag = "3")]
    pub symbol_roles: i32,
    #[prost(string, repeated, tag = "4")]
    pub override_documentation: Vec<String>,
    #[prost(int32, tag = "5")]
    pub syntax_kind: i32,
    #[prost(message, repeated, tag = "6")]
    pub diagnostics: Vec<Diagnostic>,
    #[prost(int32, repeated, tag = "7")]
    pub enclosing_range: Vec<i32>,
    #[prost(message, optional, tag = "10")]
    pub single_line_enclosing_range: Option<SingleLineRange>,
    #[prost(message, optional, tag = "11")]
    pub multi_line_enclosing_range: Option<MultiLineRange>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Diagnostic {
    #[prost(int32, tag = "1")]
    pub severity: i32,
    #[prost(string, tag = "2")]
    pub code: String,
    #[prost(string, tag = "3")]
    pub message: String,
    #[prost(string, tag = "4")]
    pub source: String,
    #[prost(int32, repeated, tag = "5")]
    pub tags: Vec<i32>,
}

/// `SymbolRole` bits.
pub mod role {
    pub const DEFINITION: i32 = 0x1;
    pub const IMPORT: i32 = 0x2;
    pub const WRITE_ACCESS: i32 = 0x4;
    pub const READ_ACCESS: i32 = 0x8;
    pub const GENERATED: i32 = 0x10;
    pub const TEST: i32 = 0x20;
    pub const FORWARD_DEFINITION: i32 = 0x40;
}

/// `PositionEncoding` values.
pub mod encoding {
    pub const UNSPECIFIED: i32 = 0;
    pub const UTF8: i32 = 1;
    pub const UTF16: i32 = 2;
    pub const UTF32: i32 = 3;
}

impl Occurrence {
    /// The occurrence's range as `[start_line, start_char, end_line, end_char]`, from the
    /// typed fields or the packed one. `None` for a missing, negative or reversed range.
    pub fn range4(&self) -> Option<[u32; 4]> {
        range4(
            &self.range,
            self.single_line_range.as_ref(),
            self.multi_line_range.as_ref(),
        )
    }

    /// The enclosing range, in the same form as [`Occurrence::range4`].
    pub fn enclosing4(&self) -> Option<[u32; 4]> {
        range4(
            &self.enclosing_range,
            self.single_line_enclosing_range.as_ref(),
            self.multi_line_enclosing_range.as_ref(),
        )
    }
}

fn range4(
    packed: &[i32],
    single: Option<&SingleLineRange>,
    multi: Option<&MultiLineRange>,
) -> Option<[u32; 4]> {
    let r = if let Some(m) = multi {
        [m.start_line, m.start_character, m.end_line, m.end_character]
    } else if let Some(s) = single {
        [s.line, s.start_character, s.line, s.end_character]
    } else {
        match *packed {
            [l, s, e] => [l, s, l, e],
            [sl, s, el, e] => [sl, s, el, e],
            _ => return None,
        }
    };
    let [a, b, c, d] = r.map(|v| u32::try_from(v).ok());
    let r = [a?, b?, c?, d?];
    ((r[0], r[1]) <= (r[2], r[3])).then_some(r)
}
