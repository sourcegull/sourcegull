//! Processing one SCIP index into a code graph file.

use std::io::Read;
use std::path::Path;

use crate::graph::{GraphError, GraphSummary, GraphWriter};
use crate::stream::{DecodeError, IndexReader, Item};

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// The index cannot be read: the upload is bad, retrying will not help.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// Writing the code graph file failed.
    #[error("writing the code graph: {0}")]
    Graph(#[from] GraphError),
    #[error("canceled")]
    Canceled,
}

/// The result of [`process_index`].
#[derive(Debug, Clone, Default)]
pub struct Processed {
    /// From the index's metadata.
    pub tool_name: String,
    pub tool_version: String,
    /// Documents skipped for a path outside the repository.
    pub skipped_documents: u32,
    pub graph: GraphSummary,
}

/// Joins an upload root (`""` or `dir/`) and a document's `relative_path` into a
/// repository-relative path; `None` when it leaves the repository or is empty.
pub fn document_path(root: &str, relative: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in root.split('/').chain(relative.split(['/', '\\'])) {
        match seg {
            "" | "." => {}
            ".." => return None,
            s => parts.push(s),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Reads the SCIP index from `input` (gzip or not) and writes its code graph to `out`.
/// `root` is the upload's root. `canceled` is polled between documents.
pub fn process_index(
    input: impl Read + Send + 'static,
    root: &str,
    out: &Path,
    tmp_dir: &Path,
    canceled: &dyn Fn() -> bool,
) -> Result<Processed, ProcessError> {
    let mut reader = IndexReader::open(input).map_err(DecodeError::Io)?;
    let mut writer = GraphWriter::create(out, tmp_dir)?;
    let mut result = Processed::default();
    while let Some(item) = reader.next_item()? {
        if canceled() {
            return Err(ProcessError::Canceled);
        }
        match item {
            Item::Metadata(m) => {
                if let Some(t) = m.tool_info {
                    result.tool_name = t.name;
                    result.tool_version = t.version;
                }
            }
            Item::Document(d) => match document_path(root, &d.relative_path) {
                Some(path) => {
                    writer.add_document(&path, &d)?;
                }
                None => {
                    result.skipped_documents += 1;
                    tracing::debug!(
                        path = d.relative_path,
                        "skipping a document outside the repository"
                    );
                }
            },
            Item::ExternalSymbol(s) => writer.add_external_symbol(&s)?,
        }
    }
    result.graph = writer.finish()?;
    Ok(result)
}
