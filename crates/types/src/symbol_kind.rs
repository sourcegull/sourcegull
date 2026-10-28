//! Symbol kinds: the LSP `SymbolKind` values, which the search API and
//! GraphQL schema name in upper case (`FUNCTION`, `STRUCT`, …).

/// A symbol kind, numbered as LSP's `SymbolKind` (0 is unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum SymbolKind {
    Unknown = 0,
    File = 1,
    Module = 2,
    Namespace = 3,
    Package = 4,
    Class = 5,
    Method = 6,
    Property = 7,
    Field = 8,
    Constructor = 9,
    Enum = 10,
    Interface = 11,
    Function = 12,
    Variable = 13,
    Constant = 14,
    String = 15,
    Number = 16,
    Boolean = 17,
    Array = 18,
    Object = 19,
    Key = 20,
    Null = 21,
    EnumMember = 22,
    Struct = 23,
    Event = 24,
    Operator = 25,
    /// Also used for type aliases, which LSP has no kind for.
    TypeParameter = 26,
}

const ALL: [SymbolKind; 27] = [
    SymbolKind::Unknown,
    SymbolKind::File,
    SymbolKind::Module,
    SymbolKind::Namespace,
    SymbolKind::Package,
    SymbolKind::Class,
    SymbolKind::Method,
    SymbolKind::Property,
    SymbolKind::Field,
    SymbolKind::Constructor,
    SymbolKind::Enum,
    SymbolKind::Interface,
    SymbolKind::Function,
    SymbolKind::Variable,
    SymbolKind::Constant,
    SymbolKind::String,
    SymbolKind::Number,
    SymbolKind::Boolean,
    SymbolKind::Array,
    SymbolKind::Object,
    SymbolKind::Key,
    SymbolKind::Null,
    SymbolKind::EnumMember,
    SymbolKind::Struct,
    SymbolKind::Event,
    SymbolKind::Operator,
    SymbolKind::TypeParameter,
];

impl SymbolKind {
    /// The kind numbered `n`; unknown numbers are [`SymbolKind::Unknown`].
    pub fn from_u8(n: u8) -> SymbolKind {
        ALL.get(usize::from(n))
            .copied()
            .unwrap_or(SymbolKind::Unknown)
    }

    /// The upper-case name used on the wire, e.g. `ENUMMEMBER`.
    pub fn name(self) -> &'static str {
        match self {
            SymbolKind::Unknown => "UNKNOWN",
            SymbolKind::File => "FILE",
            SymbolKind::Module => "MODULE",
            SymbolKind::Namespace => "NAMESPACE",
            SymbolKind::Package => "PACKAGE",
            SymbolKind::Class => "CLASS",
            SymbolKind::Method => "METHOD",
            SymbolKind::Property => "PROPERTY",
            SymbolKind::Field => "FIELD",
            SymbolKind::Constructor => "CONSTRUCTOR",
            SymbolKind::Enum => "ENUM",
            SymbolKind::Interface => "INTERFACE",
            SymbolKind::Function => "FUNCTION",
            SymbolKind::Variable => "VARIABLE",
            SymbolKind::Constant => "CONSTANT",
            SymbolKind::String => "STRING",
            SymbolKind::Number => "NUMBER",
            SymbolKind::Boolean => "BOOLEAN",
            SymbolKind::Array => "ARRAY",
            SymbolKind::Object => "OBJECT",
            SymbolKind::Key => "KEY",
            SymbolKind::Null => "NULL",
            SymbolKind::EnumMember => "ENUMMEMBER",
            SymbolKind::Struct => "STRUCT",
            SymbolKind::Event => "EVENT",
            SymbolKind::Operator => "OPERATOR",
            SymbolKind::TypeParameter => "TYPEPARAMETER",
        }
    }

    /// The kind named `name` in any case, as in `select:symbol.function`.
    pub fn from_name(name: &str) -> Option<SymbolKind> {
        ALL.into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_names_round_trip() {
        for (i, k) in ALL.iter().enumerate() {
            assert_eq!(*k as usize, i);
            assert_eq!(SymbolKind::from_u8(i as u8), *k);
            assert_eq!(SymbolKind::from_name(&k.name().to_lowercase()), Some(*k));
        }
        assert_eq!(SymbolKind::from_u8(200), SymbolKind::Unknown);
    }
}
