//! Compiles `proto/*.proto` with `protox` (a pure-Rust protobuf compiler, so no
//! `protoc` is needed on build machines) and generates prost/tonic code from
//! the resulting file descriptor set.

use std::path::PathBuf;

const PROTOS: &[&str] = &["repo.proto"];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let files: Vec<PathBuf> = PROTOS.iter().map(|p| proto_dir.join(p)).collect();
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
    }
    let fds = protox::compile(&files, [&proto_dir])?;
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .bytes(".sourcegull.repo.v1.ReadFileResponse.content")
        .bytes(".sourcegull.repo.v1.ArchiveResponse.chunk")
        .bytes(".sourcegull.repo.v1.UploadPackRequest.body")
        .bytes(".sourcegull.repo.v1.UploadPackResponse.chunk")
        .bytes(".sourcegull.repo.v1.CopyRepoResponse.chunk")
        .bytes(".sourcegull.repo.v1.CreateCommitFromPatchRequest.patch")
        .bytes(".sourcegull.repo.v1.ApplyPatchRequest.patch")
        // Implemented by hand in lib.rs so the token never reaches logs.
        .skip_debug([".sourcegull.repo.v1.PushCredentials"])
        .compile_fds(fds)?;
    Ok(())
}
