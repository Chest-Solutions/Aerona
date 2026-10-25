//! The one binary an operator runs.
//!
//! Kept deliberately small: parse the arguments, build a runtime, hand over to
//! [`aerona::serve`]. The reason it is not `#[tokio::main]` is the runtime builder — on a
//! shared host (a game panel, a 40-core box with a 1 CPU quota) tokio sizes its thread
//! pool from the *machine*, not from the cgroup, and a dashboard that is not answering
//! because 40 idle worker threads are fighting a cpuset for a timeslice is a bad way to
//! find that out at 3 a.m. `AERONA_THREADS` clamps it.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let opts = match aerona::cli::Options::parse(args, root) {
        Ok(opts) => opts,
        Err(e) => {
            // Before the logger exists, because a bad argument is precisely the case
            // where the config may not be loadable yet.
            eprintln!("aerona: {e}");
            return ExitCode::from(2);
        }
    };

    let runtime = match runtime().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("aerona: could not start the async runtime: {e}");
            return ExitCode::from(2);
        }
    };

    match runtime.block_on(aerona::serve::dispatch(&opts)) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("aerona: {e}");
            eprintln!(
                "aerona: `aerona check` explains the configuration, `aerona doctor` adds the \
                 port and Discord; `python3 main.py check` explains the install"
            );
            aerona::serve::exit_code(&e)
        }
    }
}

/// The runtime, sized for the box we were actually given.
fn runtime() -> tokio::runtime::Builder {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if let Some(threads) = std::env::var("AERONA_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        // Clamped rather than trusted: `AERONA_THREADS=0` would leave a runtime that can
        // never advance a future, which is a confusing way to hang.
        builder.worker_threads(threads.clamp(1, 64));
    }
    builder
}
