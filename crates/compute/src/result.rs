//! Compute results (`MatchContext`, `Text`, `TextExtra`): a
//! `results` event of `/.api/compute/stream` is a JSON array of them.

use serde::Serialize;
use serde::ser::{SerializeMap, SerializeStruct};

/// A position: byte offset in the file, 0-based line and byte column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Location {
    pub offset: i64,
    pub line: i64,
    pub column: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Range {
    pub start: Location,
    pub end: Location,
}

/// A captured value and where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Data {
    pub value: String,
    pub range: Range,
}

/// Capture groups by name (`1`, `2`, … for unnamed ones), in group order.
/// Serialized as a JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Environment(pub Vec<(String, Data)>);

impl Environment {
    pub fn get(&self, name: &str) -> Option<&Data> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, d)| d)
    }
}

impl Serialize for Environment {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

/// One match of a match-only query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComputeMatch {
    pub value: String,
    pub range: Range,
    pub environment: Environment,
}

/// Where a result comes from. The stream's wire format carries it for match
/// contexts, `output.extra` (repository only) and `replace-in-place`; GraphQL
/// exposes it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Origin {
    pub repository_id: i32,
    pub repository: String,
    /// The commit searched or matched; empty when unknown.
    pub commit: String,
    /// The file path; empty for repository and commit results.
    pub path: String,
}

/// The matches in one file (`ComputeMatchContext`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchContext {
    pub matches: Vec<ComputeMatch>,
    pub origin: Origin,
}

impl Serialize for MatchContext {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("MatchContext", 5)?;
        st.serialize_field("matches", &self.matches)?;
        st.serialize_field("path", &self.origin.path)?;
        st.serialize_field("repositoryID", &self.origin.repository_id)?;
        st.serialize_field("repository", &self.origin.repository)?;
        st.serialize_field("commit", &self.origin.commit)?;
        st.end()
    }
}

/// Computed text (`ComputeText`): `kind` is `output` or `replace-in-place`.
/// On the wire, `output.extra` adds the repository and `replace-in-place`
/// adds the repository, commit and path of the rewritten file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub value: String,
    pub kind: String,
    pub origin: Origin,
    /// `output.extra`: the wire format carries the repository.
    pub extra: bool,
}

impl Serialize for Text {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let replace = self.kind == "replace-in-place";
        let mut st = s.serialize_struct("Text", 6)?;
        st.serialize_field("value", &self.value)?;
        st.serialize_field("kind", &self.kind)?;
        if self.extra || replace {
            st.serialize_field("repositoryID", &self.origin.repository_id)?;
            st.serialize_field("repository", &self.origin.repository)?;
        }
        if replace {
            st.serialize_field("commit", &self.origin.commit)?;
            st.serialize_field("path", &self.origin.path)?;
        }
        st.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ComputeResult {
    MatchContext(MatchContext),
    Text(Text),
}

impl ComputeResult {
    pub fn origin(&self) -> &Origin {
        match self {
            ComputeResult::MatchContext(m) => &m.origin,
            ComputeResult::Text(t) => &t.origin,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes() {
        let origin = Origin {
            repository_id: 7,
            repository: "r".into(),
            commit: "c".into(),
            path: "p".into(),
        };
        let loc = Location {
            offset: 1,
            line: 0,
            column: 1,
        };
        let range = Range {
            start: loc,
            end: loc,
        };
        let mc = ComputeResult::MatchContext(MatchContext {
            matches: vec![ComputeMatch {
                value: "ab".into(),
                range,
                environment: Environment(vec![(
                    "1".into(),
                    Data {
                        value: "b".into(),
                        range,
                    },
                )]),
            }],
            origin: origin.clone(),
        });
        let j = serde_json::to_value(&mc).unwrap();
        assert_eq!(j["path"], "p");
        assert_eq!(j["repositoryID"], 7);
        assert_eq!(j["commit"], "c");
        assert_eq!(j["matches"][0]["environment"]["1"]["value"], "b");
        assert_eq!(j["matches"][0]["range"]["start"]["offset"], 1);

        let mut text = Text {
            value: "v".into(),
            kind: "output".into(),
            origin,
            extra: false,
        };
        assert_eq!(
            serde_json::to_string(&ComputeResult::Text(text.clone())).unwrap(),
            r#"{"value":"v","kind":"output"}"#
        );
        text.extra = true;
        assert_eq!(
            serde_json::to_string(&ComputeResult::Text(text.clone())).unwrap(),
            r#"{"value":"v","kind":"output","repositoryID":7,"repository":"r"}"#
        );
        text.extra = false;
        text.kind = "replace-in-place".into();
        assert_eq!(
            serde_json::to_string(&ComputeResult::Text(text)).unwrap(),
            r#"{"value":"v","kind":"replace-in-place","repositoryID":7,"repository":"r","commit":"c","path":"p"}"#
        );
    }
}
