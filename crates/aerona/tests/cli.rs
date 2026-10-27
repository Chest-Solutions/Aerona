//! The contract the python launcher depends on, tested by running the real binary.
//!
//! These are the only tests in the workspace that spawn the shipped executable, and they
//! exist because of a specific bug: `dispatch` used to load `Config` before it looked at
//! the command, so `--version` on a box with no `.env` and no token exited with a config
//! error. `main.py` decides whether a prebuilt binary is usable for this platform by
//! running `--version` in exactly that state, so the failure mode was "the static binary
//! we shipped looks like a broken binary, and the launcher reinstalls a toolchain".
//!
//! `env_clear()` plus a temp `current_dir` is the point of the test: it must pass with no
//! environment and nothing to find.

use std::path::Path;
use std::process::Command;

fn run(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aerona"))
        .args(args)
        .current_dir(cwd)
        .env_clear()
        // `sh` needs a PATH and cargo's test harness gives us one; the binary itself does
        // not, and passing it through would let a stray AERONA_* in the developer's shell
        // change what "no configuration" means.
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("the aerona binary should be executable")
}

#[test]
fn version_and_help_answer_without_any_configuration() {
    let empty = std::env::temp_dir();
    for arg in ["--version", "-V"] {
        let out = run(&[arg], &empty);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "{arg} exited {:?} with stderr {stderr:?}",
            out.status.code()
        );
        assert!(
            stdout.starts_with("aerona 1.") && stdout.contains("(api v1, worker protocol v"),
            "{arg} printed {stdout:?}"
        );
        assert!(stderr.is_empty(), "{arg} wrote to stderr: {stderr:?}");
    }

    for arg in ["--help", "-h"] {
        let out = run(&[arg], &empty);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{arg} exited {:?}", out.status.code());
        for expected in ["run", "check", "doctor", "--port"] {
            assert!(stdout.contains(expected), "{arg} usage lacks {expected:?}");
        }
    }
}

#[test]
fn a_missing_token_is_named_and_not_guessed() {
    // The other half of the contract: with no `.env` and no token, `run` must fail *now*,
    // loudly, with the key name in it - not bind the port and then die on the worker.
    let dir = std::env::temp_dir().join(format!("aerona-cli-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = run(&["check"], &dir);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "check must not pass with no token");
    assert!(
        combined.contains("AERONA_TOKEN"),
        "the message must name the key: {combined:?}"
    );
    assert!(!combined.contains("panicked"), "{combined}");
    let _ = std::fs::remove_dir_all(&dir);
}
