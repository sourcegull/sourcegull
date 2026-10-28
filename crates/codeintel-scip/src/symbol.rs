//! SCIP symbol strings: `local <id>`, or `<scheme> <manager> <name> <version> <descriptors>`
//! where a space inside a field is written twice and an empty field is `.`.

use sha2::{Digest, Sha256};

/// Whether `symbol` is document-local (`local …`).
pub fn is_local(symbol: &str) -> bool {
    symbol.starts_with("local ")
}

/// The package a global symbol belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Package {
    pub scheme: String,
    pub manager: String,
    pub name: String,
    pub version: String,
}

/// The package of a global symbol; `None` for local or malformed symbols.
pub fn package(symbol: &str) -> Option<Package> {
    if is_local(symbol) {
        return None;
    }
    let mut rest = symbol;
    let mut fields: [String; 4] = Default::default();
    for field in &mut fields {
        let (value, tail) = next_field(rest)?;
        *field = value;
        rest = tail;
    }
    if rest.is_empty() {
        return None;
    }
    let [scheme, manager, name, version] = fields;
    let dot = |s: String| if s == "." { String::new() } else { s };
    Some(Package {
        scheme,
        manager: dot(manager),
        name: dot(name),
        version: dot(version),
    })
}

/// Splits one space-terminated field (with `  ` as an escaped space) off `s`.
fn next_field(s: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut start = 0;
    while i < bytes.len() {
        if bytes[i] == b' ' {
            if bytes.get(i + 1) == Some(&b' ') {
                out.push_str(&s[start..=i]);
                i += 2;
                start = i;
                continue;
            }
            out.push_str(&s[start..i]);
            return (!out.is_empty()).then(|| (out, &s[i + 1..]));
        }
        i += 1;
    }
    None
}

/// The byte length of the space-terminated field at the start of `s` (`  ` is an escaped
/// space), without its terminating space; `None` when it is empty or unterminated.
fn field_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b' ' {
            if b.get(i + 1) == Some(&b' ') {
                i += 2;
                continue;
            }
            return (i > 0).then_some(i);
        }
        i += 1;
    }
    None
}

/// `symbol` with its package version replaced by `version`; `None` for local or malformed
/// symbols.
pub fn with_version(symbol: &str, version: &str) -> Option<String> {
    if is_local(symbol) {
        return None;
    }
    let mut pos = 0;
    for _ in 0..3 {
        pos += field_len(&symbol[pos..])? + 1;
    }
    let rest = &symbol[pos + field_len(&symbol[pos..])? + 1..];
    if rest.is_empty() {
        return None;
    }
    let version = if version.is_empty() {
        ".".to_owned()
    } else {
        version.replace(' ', "  ")
    };
    Some(format!("{}{version} {rest}", &symbol[..pos]))
}

/// The 64-bit hash identifying a symbol in `codeintel_symbol_postings`: the first eight bytes
/// of its SHA-256.
pub fn symbol_hash(symbol: &str) -> i64 {
    let digest = Sha256::digest(symbol.as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages() {
        assert_eq!(
            package("scip-go gomod github.com/a/b v1.2.3 `github.com/a/b`/Foo#"),
            Some(Package {
                scheme: "scip-go".into(),
                manager: "gomod".into(),
                name: "github.com/a/b".into(),
                version: "v1.2.3".into(),
            })
        );
        assert_eq!(
            package("rust-analyzer cargo my  crate . x/y()."),
            Some(Package {
                scheme: "rust-analyzer".into(),
                manager: "cargo".into(),
                name: "my crate".into(),
                version: String::new(),
            })
        );
        assert_eq!(package("local 12"), None);
        assert_eq!(package("scheme only"), None);
        assert_ne!(symbol_hash("a"), symbol_hash("b"));
    }

    #[test]
    fn versions_replaced() {
        assert_eq!(
            with_version(
                "scip-go gomod example.com/m v1.0.54 `example.com/m/pkg`/F().",
                "v1.1.0"
            )
            .as_deref(),
            Some("scip-go gomod example.com/m v1.1.0 `example.com/m/pkg`/F().")
        );
        assert_eq!(
            with_version("rust-analyzer cargo my  crate 1.0 x/y().", "").as_deref(),
            Some("rust-analyzer cargo my  crate . x/y().")
        );
        assert_eq!(
            with_version("a b c . d#", "2 beta").as_deref(),
            Some("a b c 2  beta d#")
        );
        assert_eq!(with_version("local 3", "v1"), None);
        assert_eq!(with_version("a b c v1", "v2"), None);
        let s = "scip-go gomod example.com/m v1 `example.com/m`/F().";
        assert_eq!(
            package(&with_version(s, "v2").unwrap()).unwrap().version,
            "v2"
        );
    }
}
