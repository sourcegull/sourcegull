//! Bounded, shared cache of parsed definitions for unindexed revisions.

use std::sync::Arc;

use sourcegull_gitstore::ObjectId;
use sourcegull_symbols::Symbol;

#[derive(Clone)]
pub(crate) struct SymbolCache {
    cache: moka::sync::Cache<(ObjectId, String, u32), Arc<Vec<Symbol>>>,
}

impl SymbolCache {
    pub(crate) fn new(max_bytes: u64) -> Self {
        Self {
            cache: moka::sync::Cache::builder()
                .max_capacity(max_bytes)
                .weigher(|key: &(ObjectId, String, u32), value: &Arc<Vec<Symbol>>| {
                    let bytes = std::mem::size_of_val(key)
                        + key.1.len()
                        + std::mem::size_of::<Vec<Symbol>>()
                        + value
                            .iter()
                            .map(|symbol| {
                                std::mem::size_of::<Symbol>()
                                    + symbol.name.len()
                                    + symbol.container.as_ref().map_or(0, String::len)
                            })
                            .sum::<usize>();
                    u32::try_from(bytes).unwrap_or(u32::MAX)
                })
                .build(),
        }
    }

    pub(crate) fn get_or_extract(
        &self,
        oid: ObjectId,
        lang: &str,
        content: &[u8],
    ) -> Arc<Vec<Symbol>> {
        self.cache
            .get_with((oid, lang.to_owned(), sourcegull_symbols::VERSION), || {
                Arc::new(sourcegull_symbols::extract(lang, content))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_definitions_for_blob_and_language() {
        let cache = SymbolCache::new(1024 * 1024);
        let oid = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        let content = b"func Run() {}\n";
        let first = cache.get_or_extract(oid, "go", content);
        assert_eq!(first.len(), 1);
        // This deliberately supplies different bytes with the same OID. A hit
        // proves the parsed definitions are reused, rather than parsed again.
        let second = cache.get_or_extract(oid, "go", b"func Other() {}\n");
        assert!(Arc::ptr_eq(&first, &second));
        let other_language = cache.get_or_extract(oid, "python", b"def other(): pass\n");
        assert!(!Arc::ptr_eq(&first, &other_language));
    }
}
