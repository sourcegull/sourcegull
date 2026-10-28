//! Highlight kinds and the mapping from tree-sitter capture names and
//! TextMate scopes onto them.

use serde::{Deserialize, Serialize};

/// What a highlighted range is. The numeric values are part of the packed
/// wire format ([`crate::pack`]); append new kinds, never renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(u8)]
pub enum SyntaxKind {
    /// Comments.
    Comment = 1,
    /// Keywords and storage modifiers.
    Keyword = 2,
    /// String literals.
    String = 3,
    /// Escape sequences and interpolation markers inside strings.
    StringEscape = 4,
    /// Regular expression and other special literals.
    Regex = 5,
    /// Numeric literals.
    Number = 6,
    /// Named constants.
    Constant = 7,
    /// Built-in constants (`true`, `null`, `None`).
    ConstantBuiltin = 8,
    /// Function and method names.
    Function = 9,
    /// Built-in functions.
    FunctionBuiltin = 10,
    /// Macros.
    Macro = 11,
    /// Type names.
    Type = 12,
    /// Built-in types (`int`, `str`).
    TypeBuiltin = 13,
    /// Constructors.
    Constructor = 14,
    /// Built-in variables (`self`, `this`).
    VariableBuiltin = 15,
    /// Parameters.
    Parameter = 16,
    /// Fields and properties.
    Property = 17,
    /// Operators.
    Operator = 18,
    /// Special punctuation (interpolation delimiters, markup markers).
    Punctuation = 19,
    /// Markup tags.
    Tag = 20,
    /// Attributes, annotations, decorators.
    Attribute = 21,
    /// Modules and namespaces.
    Namespace = 22,
    /// Labels.
    Label = 23,
    /// Markup headings.
    Heading = 24,
    /// Emphasis.
    Emphasis = 25,
    /// Strong emphasis.
    Strong = 26,
    /// Links and URLs.
    Link = 27,
    /// Inline and block code in prose.
    Code = 28,
}

impl SyntaxKind {
    /// Every kind, in numeric order.
    pub const ALL: [SyntaxKind; 28] = [
        SyntaxKind::Comment,
        SyntaxKind::Keyword,
        SyntaxKind::String,
        SyntaxKind::StringEscape,
        SyntaxKind::Regex,
        SyntaxKind::Number,
        SyntaxKind::Constant,
        SyntaxKind::ConstantBuiltin,
        SyntaxKind::Function,
        SyntaxKind::FunctionBuiltin,
        SyntaxKind::Macro,
        SyntaxKind::Type,
        SyntaxKind::TypeBuiltin,
        SyntaxKind::Constructor,
        SyntaxKind::VariableBuiltin,
        SyntaxKind::Parameter,
        SyntaxKind::Property,
        SyntaxKind::Operator,
        SyntaxKind::Punctuation,
        SyntaxKind::Tag,
        SyntaxKind::Attribute,
        SyntaxKind::Namespace,
        SyntaxKind::Label,
        SyntaxKind::Heading,
        SyntaxKind::Emphasis,
        SyntaxKind::Strong,
        SyntaxKind::Link,
        SyntaxKind::Code,
    ];

    /// Decodes a packed kind.
    pub fn from_u8(v: u8) -> Option<Self> {
        Self::ALL.get(usize::from(v).checked_sub(1)?).copied()
    }

    /// CSS class suffix used by the HTML renderer and the UI (`hl-keyword`).
    pub fn css_class(self) -> &'static str {
        match self {
            SyntaxKind::Comment => "comment",
            SyntaxKind::Keyword => "keyword",
            SyntaxKind::String => "string",
            SyntaxKind::StringEscape => "string-escape",
            SyntaxKind::Regex => "regex",
            SyntaxKind::Number => "number",
            SyntaxKind::Constant => "constant",
            SyntaxKind::ConstantBuiltin => "constant-builtin",
            SyntaxKind::Function => "function",
            SyntaxKind::FunctionBuiltin => "function-builtin",
            SyntaxKind::Macro => "macro",
            SyntaxKind::Type => "type",
            SyntaxKind::TypeBuiltin => "type-builtin",
            SyntaxKind::Constructor => "constructor",
            SyntaxKind::VariableBuiltin => "variable-builtin",
            SyntaxKind::Parameter => "parameter",
            SyntaxKind::Property => "property",
            SyntaxKind::Operator => "operator",
            SyntaxKind::Punctuation => "punctuation",
            SyntaxKind::Tag => "tag",
            SyntaxKind::Attribute => "attribute",
            SyntaxKind::Namespace => "namespace",
            SyntaxKind::Label => "label",
            SyntaxKind::Heading => "heading",
            SyntaxKind::Emphasis => "emphasis",
            SyntaxKind::Strong => "strong",
            SyntaxKind::Link => "link",
            SyntaxKind::Code => "code",
        }
    }
}

/// Capture names configured on every tree-sitter grammar. Captures not
/// listed here (plain `variable`, most `punctuation.*`) produce no ranges,
/// which keeps payloads small: the viewer shows them in the default color.
pub(crate) const CAPTURE_NAMES: &[&str] = &[
    "comment",
    "keyword",
    "string",
    "string.escape",
    "string.special",
    "string.regex",
    "escape",
    "number",
    "float",
    "boolean",
    "constant",
    "constant.builtin",
    "function",
    "function.builtin",
    "function.macro",
    "function.method",
    "method",
    "type",
    "type.builtin",
    "constructor",
    "variable.builtin",
    "variable.parameter",
    "parameter",
    "property",
    "field",
    "operator",
    "punctuation.special",
    "tag",
    "attribute",
    "namespace",
    "module",
    "label",
    "embedded",
    "text.title",
    "text.emphasis",
    "text.strong",
    "text.uri",
    "text.reference",
    "text.literal",
    "markup.heading",
    "markup.italic",
    "markup.bold",
    "markup.link",
    "markup.raw",
];

/// Maps a configured capture name to a kind (`None` for `embedded`, which
/// only marks injected regions).
pub(crate) fn kind_for_capture(name: &str) -> Option<SyntaxKind> {
    use SyntaxKind as K;
    Some(match name {
        "comment" => K::Comment,
        "keyword" => K::Keyword,
        "string" => K::String,
        "string.escape" | "escape" => K::StringEscape,
        "string.special" | "string.regex" => K::Regex,
        "number" | "float" => K::Number,
        "boolean" | "constant.builtin" => K::ConstantBuiltin,
        "constant" => K::Constant,
        "function" | "function.method" | "method" => K::Function,
        "function.builtin" => K::FunctionBuiltin,
        "function.macro" => K::Macro,
        "type" => K::Type,
        "type.builtin" => K::TypeBuiltin,
        "constructor" => K::Constructor,
        "variable.builtin" => K::VariableBuiltin,
        "variable.parameter" | "parameter" => K::Parameter,
        "property" | "field" => K::Property,
        "operator" => K::Operator,
        "punctuation.special" => K::Punctuation,
        "tag" => K::Tag,
        "attribute" => K::Attribute,
        "namespace" | "module" => K::Namespace,
        "label" => K::Label,
        "text.title" | "markup.heading" => K::Heading,
        "text.emphasis" | "markup.italic" => K::Emphasis,
        "text.strong" | "markup.bold" => K::Strong,
        "text.uri" | "text.reference" | "markup.link" => K::Link,
        "text.literal" | "markup.raw" => K::Code,
        _ => return None,
    })
}

/// Maps a TextMate scope (syntect) to a kind by prefix, most specific first.
#[cfg_attr(not(feature = "syntect"), allow(dead_code))]
pub(crate) fn kind_for_scope(scope: &str) -> Option<SyntaxKind> {
    use SyntaxKind as K;
    const RULES: &[(&str, SyntaxKind)] = &[
        ("comment", K::Comment),
        ("constant.character.escape", K::StringEscape),
        ("string.regexp", K::Regex),
        ("string", K::String),
        ("constant.numeric", K::Number),
        ("constant.language", K::ConstantBuiltin),
        ("constant", K::Constant),
        ("entity.name.function", K::Function),
        ("support.function", K::FunctionBuiltin),
        ("variable.function", K::Function),
        ("entity.name.type", K::Type),
        ("entity.name.class", K::Type),
        ("entity.name.struct", K::Type),
        ("entity.name.enum", K::Type),
        ("entity.name.namespace", K::Namespace),
        ("entity.name.module", K::Namespace),
        ("entity.name.tag", K::Tag),
        ("entity.name.label", K::Label),
        ("entity.other.attribute-name", K::Attribute),
        ("entity.other.inherited-class", K::Type),
        ("support.type", K::TypeBuiltin),
        ("support.class", K::Type),
        ("storage.type", K::Keyword),
        ("storage.modifier", K::Keyword),
        ("keyword.operator", K::Operator),
        ("keyword", K::Keyword),
        ("variable.language", K::VariableBuiltin),
        ("variable.parameter", K::Parameter),
        ("variable.other.member", K::Property),
        ("meta.attribute", K::Attribute),
        ("markup.heading", K::Heading),
        ("markup.italic", K::Emphasis),
        ("markup.bold", K::Strong),
        ("markup.underline.link", K::Link),
        ("markup.raw", K::Code),
        ("punctuation.definition.heading", K::Heading),
        ("punctuation.section.embedded", K::Punctuation),
    ];
    RULES
        .iter()
        .find(|(prefix, _)| {
            scope.starts_with(prefix)
                && scope
                    .as_bytes()
                    .get(prefix.len())
                    .is_none_or(|b| *b == b'.')
        })
        .map(|(_, k)| *k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_roundtrip_through_u8() {
        for k in SyntaxKind::ALL {
            assert_eq!(SyntaxKind::from_u8(k as u8), Some(k));
        }
        assert_eq!(SyntaxKind::from_u8(0), None);
        assert_eq!(SyntaxKind::from_u8(200), None);
    }

    #[test]
    fn every_capture_name_maps() {
        for name in CAPTURE_NAMES {
            if *name != "embedded" {
                assert!(kind_for_capture(name).is_some(), "{name}");
            }
        }
    }

    #[test]
    fn scope_prefixes_match_on_dot_boundaries() {
        assert_eq!(
            kind_for_scope("comment.line.double-slash.rust"),
            Some(SyntaxKind::Comment)
        );
        assert_eq!(
            kind_for_scope("keyword.operator.arithmetic"),
            Some(SyntaxKind::Operator)
        );
        assert_eq!(
            kind_for_scope("keyword.control.flow"),
            Some(SyntaxKind::Keyword)
        );
        assert_eq!(
            kind_for_scope("constant.numeric.integer"),
            Some(SyntaxKind::Number)
        );
        assert_eq!(kind_for_scope("stringy"), None);
        assert_eq!(kind_for_scope("source.rust"), None);
    }
}
