//! `sourcegull`: the single server binary. See the `sourcegull` library crate for the logic.

use std::process::ExitCode;

use clap::Parser as _;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[allow(unsafe_code)]
fn main() -> ExitCode {
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
