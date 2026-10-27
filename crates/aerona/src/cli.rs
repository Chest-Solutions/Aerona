//! Argument handling: the flags are the only place this binary knows anything about
//! how it was started, and they are applied to the *environment* before the config is
//! read.
//!
//! That order is deliberate. `Config` already knows how to reconcile the process
//! environment with `.env` (environment wins), so a flag that writes one environment
//! variable can never disagree with `.env` in Rust: there is exactly one rule for
//! precedence, not one per flag.

use std::path::PathBuf;

use aerona_core::error::{Error, Result};

/// What to do, after the flags are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Build nothing, bind the port, serve the dashboard, hold the gateway.
    Run,
    /// Prove config + database + dashboard + intents, bind nothing.
    Check,
    /// Everything `check` proves, plus the three things that need the network or the
    /// filesystem: can the port be bound, is the token accepted by Discord, is the
    /// dashboard actually built.
    Doctor,
    Version,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub command: Command,
    /// Debug profile: allows a low port and a missing dashboard.
    pub dev: bool,
    /// Log our own crates at debug, whatever `.env` says.
    pub verbose: bool,
    /// `Some(false)` from `--no-bot`, `Some(true)` from `--bot`, `None` from silence.
    ///
    /// `None` means "ask the environment", which is what lets a panel set
    /// `AERONA_BOT=off` once instead of on every command line.
    pub bot: Option<bool>,
    pub port: Option<u16>,
    pub static_dir: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    /// The install root: where `.env`, `data/` and `dashboard/dist` live.
    pub root: PathBuf,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            command: Command::Run,
            dev: false,
            verbose: false,
            bot: None,
            port: None,
            static_dir: None,
            data_dir: None,
            root: PathBuf::from("."),
        }
    }
}

impl Options {
    pub fn parse<I>(args: I, root: PathBuf) -> Result<Self>
    where
        I: IntoIterator<Item = String>,
    {
        let mut out = Options {
            root,
            ..Options::default()
        };
        let mut command: Option<Command> = None;
        let mut argv = args.into_iter();
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "" => {}
                "-h" | "--help" => {
                    return Ok(Self {
                        command: Command::Help,
                        ..out
                    })
                }
                "-V" | "--version" => {
                    return Ok(Self {
                        command: Command::Version,
                        ..out
                    })
                }
                "--dev" => out.dev = true,
                "--verbose" | "-v" => out.verbose = true,
                "--no-bot" => out.bot = Some(false),
                "--bot" => out.bot = Some(true),
                "--check" => set_command(&mut command, Command::Check, &arg)?,
                "--doctor" => set_command(&mut command, Command::Doctor, &arg)?,
                "--run" => set_command(&mut command, Command::Run, &arg)?,
                other if !other.starts_with('-') => {
                    // `check` and `doctor` are different things - the second is the
                    // one that touches the network - so no alias may quietly pick the
                    // other.
                    let named = match other {
                        "run" | "start" | "serve" => Command::Run,
                        "check" => Command::Check,
                        "doctor" | "verify" => Command::Doctor,
                        "version" => Command::Version,
                        "help" => Command::Help,
                        unknown => {
                            return Err(Error::Config(format!(
                                "unknown command {unknown:?} - expected run, check, doctor, version or help"
                            )))
                        }
                    };
                    set_command(&mut command, named, &arg)?;
                }
                other if other.starts_with("--port=") => {
                    out.port = Some(parse_port(other.trim_start_matches("--port="))?);
                }
                "--port" | "-p" => {
                    // No `{other}` in the message: an or-pattern binds nothing.
                    let value = argv.next().ok_or_else(|| {
                        Error::Config("--port needs a number, e.g. --port 8080".into())
                    })?;
                    out.port = Some(parse_port(&value)?);
                }
                other if other.starts_with("--root=") => {
                    out.root = PathBuf::from(value_after(other, "--root=").to_string());
                }
                "--root" => {
                    let value = argv
                        .next()
                        .ok_or_else(|| Error::Config("--root needs a directory".into()))?;
                    out.root = PathBuf::from(value);
                }
                other if other.starts_with("--static=") => {
                    out.static_dir =
                        Some(PathBuf::from(value_after(other, "--static=").to_string()));
                }
                "--static" => {
                    let value = argv
                        .next()
                        .ok_or_else(|| Error::Config("--static needs a directory".into()))?;
                    out.static_dir = Some(PathBuf::from(value));
                }
                other if other.starts_with("--data=") => {
                    out.data_dir = Some(PathBuf::from(value_after(other, "--data=").to_string()));
                }
                "--data" => {
                    let value = argv
                        .next()
                        .ok_or_else(|| Error::Config("--data needs a directory".into()))?;
                    out.data_dir = Some(PathBuf::from(value));
                }
                unknown => {
                    return Err(Error::Config(format!(
                        "unknown argument {unknown:?}\n{}",
                        usage_line()
                    )))
                }
            }
        }
        if let Some(c) = command {
            out.command = c;
        }
        // A bare `aerona` must also honour AERONA_ENV=development, because that is how
        // a container is configured without a wrapper script editing argv.
        if !out.dev {
            out.dev = dev_from_process_env(std::env::var("AERONA_ENV").ok());
        }
        Ok(out)
    }

    /// Push the flags into the environment so `Config::load` sees them.
    ///
    /// Called once, before any config is parsed. Every value set here is one the
    /// operator could have set themselves; the flag is simply the closer hand.
    pub fn apply_to_env(&self) {
        if let Some(bot) = self.bot {
            std::env::set_var("AERONA_BOT", if bot { "true" } else { "off" });
        }
        if let Some(port) = self.port {
            std::env::set_var("AERONA_PORT", port.to_string());
        }
        if let Some(dir) = &self.static_dir {
            std::env::set_var("AERONA_STATIC_DIR", dir);
        }
        if let Some(dir) = &self.data_dir {
            std::env::set_var("AERONA_DATA_DIR", dir);
        }
        if self.dev {
            std::env::set_var("AERONA_ENV", "development");
        }
        if self.verbose {
            std::env::set_var("AERONA_LOG_LEVEL", "debug");
        }
    }

    pub fn usage() -> &'static str {
        "\
aerona - the dashboard, the API and the Discord worker in one process

usage: aerona [COMMAND] [FLAGS]

commands
  run        serve the dashboard and connect to Discord (the default)
  check      prove the config, the database and the intents; bind nothing
  doctor     check, plus the port, the dashboard files and Discord's answer to the token

flags
  --no-bot           run the dashboard with no gateway worker (also AERONA_BOT=off)
  --bot              force the worker on, whatever the environment says
  --port <n>         override AERONA_PORT (and AERONA_BASE_URL's default port)
  --root <dir>       install root: where .env, data/ and dashboard/dist live
  --data <dir>       override AERONA_DATA_DIR
  --static <dir>     override AERONA_STATIC_DIR
  --dev              allow a privileged port and a missing dashboard build
  -v, --verbose      log our own crates at debug
  -V, --version      print the version and exit
  -h, --help         print this

everything else is configured in .env - see .env.example (and `python3 main.py check`).
"
    }
}

/// The env-var half of the `--dev` flag, split out so it is testable without mutating
/// the process environment (which every other test in this binary reads).
fn dev_from_process_env(value: Option<String>) -> bool {
    value
        .map(|v| v.eq_ignore_ascii_case("development"))
        .unwrap_or(false)
}

fn usage_line() -> &'static str {
    "usage: aerona [run|check|doctor] [--no-bot] [--port N] [-h]"
}

fn set_command(held: &mut Option<Command>, want: Command, arg: &str) -> Result<()> {
    if let Some(previous) = held {
        if *previous != want {
            return Err(Error::Config(format!(
                "`{arg}` says {want:?} but the command is already {previous:?} - pick one"
            )));
        }
    }
    *held = Some(want);
    Ok(())
}

fn parse_port(raw: &str) -> Result<u16> {
    let trimmed = raw.trim();
    let value: u16 = trimmed.parse().map_err(|_| {
        Error::Config(format!(
            "--port must be a whole number between 1 and 65535 (got {trimmed:?})"
        ))
    })?;
    if value == 0 {
        return Err(Error::Config(
            "--port 0 would give the dashboard a random port, which no OAuth redirect \
             or CSRF origin check can survive"
                .into(),
        ));
    }
    Ok(value)
}

fn value_after<'a>(arg: &'a str, prefix: &str) -> &'a str {
    arg.strip_prefix(prefix).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options> {
        Options::parse(args.iter().map(|s| s.to_string()), PathBuf::from("/srv"))
    }

    #[test]
    fn bare_invocation_runs() {
        let o = parse(&[]).unwrap();
        assert_eq!(o.command, Command::Run);
        assert_eq!(o.root, PathBuf::from("/srv"));
        assert_eq!(o.bot, None, "silence means: ask the environment");
        assert!(!o.dev);
    }

    #[test]
    fn commands_may_come_first_or_as_flags() {
        assert_eq!(parse(&["check"]).unwrap().command, Command::Check);
        assert_eq!(parse(&["--check"]).unwrap().command, Command::Check);
        assert_eq!(parse(&["doctor"]).unwrap().command, Command::Doctor);
        assert_eq!(parse(&["--doctor"]).unwrap().command, Command::Doctor);
        assert_eq!(parse(&["start"]).unwrap().command, Command::Run);
        assert_eq!(parse(&["-h"]).unwrap().command, Command::Help);
        assert_eq!(parse(&["--version"]).unwrap().command, Command::Version);
    }

    #[test]
    fn usage_is_accurate_about_the_flags_it_accepts() {
        let text = Options::usage();
        for flag in [
            "--no-bot",
            "--bot",
            "--port",
            "--root",
            "--data",
            "--static",
            "--dev",
            "--verbose",
        ] {
            assert!(text.contains(flag), "usage is missing {flag}");
        }
        for command in ["run", "check", "doctor"] {
            assert!(text.contains(command), "usage is missing {command}");
        }
        assert!(text.len() < 2_000, "a usage block nobody reads is a bug");
    }

    #[test]
    fn ports_are_checked_here_not_later() {
        assert_eq!(parse(&["--port", "25565"]).unwrap().port, Some(25565));
        assert_eq!(parse(&["--port=8080"]).unwrap().port, Some(8080));
        assert_eq!(parse(&["-p", "9000"]).unwrap().port, Some(9000));
        for bad in ["0", "abc", "70000", "-1", "80.5", ""] {
            let err = parse(&["--port", bad]).unwrap_err().to_string();
            assert!(err.contains("--port"), "{bad:?} -> {err}");
        }
        assert!(
            parse(&["--port"]).is_err(),
            "a flag with no value is an error"
        );
    }

    #[test]
    fn directories_come_along() {
        let o = parse(&["--root", "/srv/a", "--data", "/d", "--static", "/s"]).unwrap();
        assert_eq!(o.root, PathBuf::from("/srv/a"));
        assert_eq!(o.data_dir, Some(PathBuf::from("/d")));
        assert_eq!(o.static_dir, Some(PathBuf::from("/s")));
        let o = parse(&["--static=/var/www/dist"]).unwrap();
        assert_eq!(o.static_dir, Some(PathBuf::from("/var/www/dist")));
    }

    #[test]
    fn nonsense_is_rejected_with_an_action() {
        let err = parse(&["--nope"]).unwrap_err().to_string();
        assert!(err.contains("unknown argument"), "{err}");
        assert!(err.contains("usage:"), "{err}");
        let err = parse(&["frobnicate"]).unwrap_err().to_string();
        assert!(err.contains("unknown command"), "{err}");
        // Two commands that disagree are refused rather than resolved by whoever
        // happened to come last.
        assert!(parse(&["check", "--run"]).is_err());
        // ...while repeating the same one is harmless, because a panel that appends
        // its own flags should not break.
        assert_eq!(
            parse(&["check", "--check"]).unwrap().command,
            Command::Check
        );
    }

    #[test]
    fn the_bot_switch_is_only_recorded_never_interpreted() {
        assert_eq!(parse(&["--no-bot"]).unwrap().bot, Some(false));
        assert_eq!(parse(&["--bot"]).unwrap().bot, Some(true));
        assert_eq!(
            parse(&["check", "--no-bot"]).unwrap(),
            Options {
                command: Command::Check,
                bot: Some(false),
                root: PathBuf::from("/srv"),
                ..Options::default()
            }
        );
    }

    #[test]
    fn dev_flag_and_env_are_the_same_switch() {
        assert!(parse(&["--dev"]).unwrap().dev);
        // `AERONA_ENV=development` is read from the environment after the loop, so a
        // container can skip a release build without anyone editing argv. Setting a
        // process env var inside a test would race every other test in this binary,
        // so `Options::dev_from_process_env` is the part under test.
        assert!(dev_from_process_env(Some("development".into())));
        assert!(
            dev_from_process_env(Some("DEVELOPMENT".into())),
            "the value a container image sets is upper-case on some bases"
        );
        assert!(!dev_from_process_env(Some("production".into())));
        assert!(!dev_from_process_env(None));
    }
}
