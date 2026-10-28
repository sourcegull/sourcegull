//! `gull code-intel …`: uploading SCIP indexes.

pub mod transfer;
pub mod upload;

pub use upload::{UploadArgs, run as upload};
