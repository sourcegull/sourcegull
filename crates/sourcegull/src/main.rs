//! `sourcegull`: the single server binary. See the `sourcegull` library crate for the logic.

use std::process::ExitCode;

use clap::Parser as _;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `calloc` for tree-sitter on mimalloc: zeroed memory, null when `n * size` overflows.
#[allow(unsafe_code)]
unsafe extern "C" fn mi_calloc(n: usize, size: usize) -> *mut std::ffi::c_void {
    match n.checked_mul(size) {
        // SAFETY: mi_zalloc takes any size and returns null or a zeroed block of that size.
        Some(total) => unsafe { libmimalloc_sys::mi_zalloc(total) },
        None => std::ptr::null_mut(),
    }
}

#[allow(unsafe_code)]
fn main() -> ExitCode {
    // tree-sitter's C code allocates through libc unless told otherwise. The release binary is
    // static musl, whose malloc serialises threads, so symbol extraction on several threads
    // would run at a fraction of its speed: give tree-sitter mimalloc, as Rust code has.
    // SAFETY: the four functions are mimalloc's (one allocator, malloc alignment, null only on
    // failure), and no tree-sitter object exists yet.
    unsafe {
        tree_sitter::set_allocator(Some(tree_sitter::Allocator {
            malloc: libmimalloc_sys::mi_malloc,
            calloc: mi_calloc,
            realloc: libmimalloc_sys::mi_realloc,
            free: libmimalloc_sys::mi_free,
        }));
    }
    let cli = sourcegull::cli::Cli::parse();
    // SAFETY: no other thread exists yet; the runtime's threads start below.
    unsafe { sourcegull::startup::capture_inline_keys() };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: starting the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(sourcegull::main(cli))
}
