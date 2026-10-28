//! Points `rust-embed` at the built UI (`ui/build`), or at an empty directory when the UI has not
//! been built, so `cargo build` works on a fresh checkout without Node.js.
//!
//! Override the location with `SOURCEGULL_UI_BUILD_DIR`.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCEGULL_UI_BUILD_DIR");

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ui_dir = manifest_dir.join("../../ui");
    let build_dir = env::var_os("SOURCEGULL_UI_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ui_dir.join("build"));

    let (folder, embedded) = if build_dir.join("index.html").is_file() {
        println!("cargo:rerun-if-changed={}", build_dir.display());
        (build_dir, true)
    } else {
        // Re-run once the UI gets built. Watching `ui/` (small without a build) catches the
        // creation of `ui/build`; watching a missing path would re-run on every build.
        if ui_dir.is_dir() {
            println!("cargo:rerun-if-changed={}", ui_dir.display());
        }
        let empty = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("empty-ui");
        std::fs::create_dir_all(&empty).expect("create empty UI dir");
        (empty, false)
    };

    let folder = folder.canonicalize().unwrap_or(folder);
    println!(
        "cargo:rustc-env=SOURCEGULL_UI_ASSETS_DIR={}",
        folder.display()
    );
    println!("cargo:rustc-env=SOURCEGULL_UI_EMBEDDED={embedded}");
}
