//! `SOURCEGULL_KEYS` is read once at startup and removed from the environment: child processes
//! never inherit it, while other variables (such as those config files reference) stay.
//!
//! The test runs itself again with the variables set, so the capture happens in a process
//! with a single thread, as in `main`.

use std::process::{Command, ExitCode};

const CHILD: &str = "SOURCEGULL_INLINE_KEYS_TEST_CHILD";
const KEYS: &str = "k1:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

fn main() -> ExitCode {
    if std::env::var_os(CHILD).is_some() {
        return child();
    }
    let status = Command::new(std::env::current_exe().expect("current exe"))
        .env(CHILD, "1")
        .env("SOURCEGULL_KEYS", KEYS)
        .env("SOURCEGULL_TEST_OTHER_SECRET", "kept")
        .status()
        .expect("run the test child");
    if status.success() {
        println!("test inline_keys ... ok");
        ExitCode::SUCCESS
    } else {
        eprintln!("test inline_keys ... FAILED ({status})");
        ExitCode::FAILURE
    }
}

#[allow(unsafe_code)]
fn child() -> ExitCode {
    // SAFETY: this process has a single thread.
    unsafe { sourcegull::startup::capture_inline_keys() };
    let keys = sourcegull::startup::inline_keys().expect("captured");
    assert_eq!(keys.as_str(), KEYS);
    assert!(std::env::var_os("SOURCEGULL_KEYS").is_none());

    let out = Command::new("sh")
        .args([
            "-c",
            r#"printf '%s|%s' "${SOURCEGULL_KEYS-unset}" "${SOURCEGULL_TEST_OTHER_SECRET-unset}""#,
        ])
        .output()
        .expect("run sh");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "unset|kept");

    // The keys load from the captured value.
    let cli = <sourcegull::cli::Cli as clap::Parser>::try_parse_from([
        "sourcegull",
        "run",
        "--database-url=postgres://x/y",
    ])
    .expect("parse");
    let sourcegull::cli::Command::Run(run) = cli.command else {
        panic!("expected run")
    };
    let source = run.server.key_source().expect("source");
    let provider = sourcegull::startup::load_keys(&source)
        .expect("load")
        .expect("keys");
    assert_eq!(
        sourcegull_crypto::KeyProvider::current_kek_id(&provider),
        "k1"
    );
    ExitCode::SUCCESS
}
