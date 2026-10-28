//! The indexer images the built-in recognizers use (third-party SCIP indexer images, pinned by
//! digest). `codeIntelAutoIndexing.indexerMap` in site config overrides them per language key.
//!
//! The digests are the multi-architecture manifest digests Docker Hub listed for the tags in the
//! comments. Where an image has a dedicated `autoindex` tag (with extra tooling such as `n` for
//! Node versions), that tag is pinned.

use std::collections::BTreeMap;

/// (language key, image) of the built-in indexers.
pub const DEFAULT_INDEXERS: &[(&str, &str)] = &[
    // sourcegraph/scip-go:v0.1.26
    (
        "go",
        "sourcegraph/scip-go@sha256:c8925f8de1b6f168eda664b6ed14ff14e59e063466209e7f335dd0f5b1746f7f",
    ),
    // sourcegraph/scip-java:0.12.3
    (
        "java",
        "sourcegraph/scip-java@sha256:2ad40207c69b0758fc0be783314fbb802232358acd648b5ecc2f870e372ee618",
    ),
    // sourcegraph/scip-python:autoindex (= v0.6.6)
    (
        "python",
        "sourcegraph/scip-python@sha256:b54e304eaaff26568d5ccd21ac29aca8c99b8466590cb127b3af74337db38d92",
    ),
    // sourcegraph/scip-typescript:autoindex
    (
        "typescript",
        "sourcegraph/scip-typescript@sha256:50fb1b855d5f4cda127b889aa2a7a32a782932d213951bfe425e0a7b88e7477d",
    ),
    // sourcegraph/scip-ruby:autoindex (= autoindex-0.5.0)
    (
        "ruby",
        "sourcegraph/scip-ruby@sha256:480eb60e27a5dd4c98bbe6c12a726f3ca42330819b9f75ae1702554ca5aeb5e6",
    ),
    // sourcegraph/scip-rust:v0.0.5 (rust-analyzer's SCIP output)
    (
        "rust",
        "sourcegraph/scip-rust@sha256:75ec8d7c7f8dc295754cf679b39683250797f951cc9686933995b285bc79e4f1",
    ),
    // sourcegraph/scip-dotnet:v0.2.14
    (
        "dotnet",
        "sourcegraph/scip-dotnet@sha256:be3ed668c2b1068bebd10e16b3a7768fa4f2e2daea277d48106200157486d3f4",
    ),
    // sourcegraph/scip-clang:latest (the image has no version tags)
    (
        "clang",
        "sourcegraph/scip-clang@sha256:c14c72512a00e5474ade07964cab4dbab7598bcdf8c382dd3a0c2eef5b690d8f",
    ),
];

/// The indexer images by language key: the defaults with site config overrides applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexerMap {
    overrides: BTreeMap<String, String>,
}

impl IndexerMap {
    pub fn new(overrides: BTreeMap<String, String>) -> Self {
        Self { overrides }
    }

    /// The image for `language`, if any.
    pub fn get(&self, language: &str) -> Option<&str> {
        self.overrides
            .get(language)
            .map(String::as_str)
            .or_else(|| {
                DEFAULT_INDEXERS
                    .iter()
                    .find(|(k, _)| *k == language)
                    .map(|(_, v)| *v)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_win() {
        let m = IndexerMap::new([("go".to_owned(), "acme/scip-go:1".to_owned())].into());
        assert_eq!(m.get("go"), Some("acme/scip-go:1"));
        assert!(
            m.get("rust")
                .unwrap()
                .starts_with("sourcegraph/scip-rust@sha256:")
        );
        assert_eq!(m.get("cobol"), None);
        for (_, image) in DEFAULT_INDEXERS {
            let digest = image.split_once("@sha256:").unwrap().1;
            assert_eq!(digest.len(), 64, "{image}");
        }
    }
}
