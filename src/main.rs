//! claude-usage: one shared, cached fetcher for Claude usage limits.
//!
//! `get`, `fetch` and `status` are built; `install` and `uninstall` still answer "not
//! implemented".

mod config;
mod creds;
mod curl;
mod detach;
mod fetch;
mod get;
mod lock;
mod parse;
mod paths;
mod schedule;
mod secret;
mod state;
mod status;
mod sys;

use std::io::Write;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

const USAGE: &str = "\
usage: claude-usage get [--refresh]
       claude-usage fetch [--now]
       claude-usage status
       claude-usage install [--timer | --no-timer] [--status]
       claude-usage uninstall [--purge]
       claude-usage --help | --version
";

/// A parsed command line.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Get { refresh: bool },
    Fetch { now: bool },
    Status,
    Install { timer: Option<bool>, status: bool },
    Uninstall { purge: bool },
    Help,
    Version,
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Get { .. } => "get",
            Command::Fetch { .. } => "fetch",
            Command::Status => "status",
            Command::Install { .. } => "install",
            Command::Uninstall { .. } => "uninstall",
            Command::Help => "--help",
            Command::Version => "--version",
        }
    }
}

/// Parses the arguments after the program name. Each flag may appear once, and only with its
/// own command.
fn parse(args: &[&str]) -> Result<Command, String> {
    let Some((&cmd, rest)) = args.split_first() else {
        return Err("no command given".into());
    };
    let allowed: &[&str] = match cmd {
        "get" => &["--refresh"],
        "fetch" => &["--now"],
        "install" => &["--timer", "--no-timer", "--status"],
        "uninstall" => &["--purge"],
        "status" => &[],
        "--help" | "-h" | "--version" if rest.is_empty() => return Ok(top_level(cmd)),
        "--help" | "-h" | "--version" => return Err(format!("{cmd} takes no arguments")),
        _ => return Err(format!("unknown command: {cmd}")),
    };
    // Help after any command asks for help, whatever else is given.
    if rest.iter().any(|&f| f == "--help" || f == "-h") {
        return Ok(Command::Help);
    }
    let mut seen: Vec<&str> = Vec::new();
    for &flag in rest {
        if !allowed.contains(&flag) {
            return Err(format!("{cmd}: unknown option: {flag}"));
        }
        if seen.contains(&flag) {
            return Err(format!("{cmd}: {flag} given twice"));
        }
        seen.push(flag);
    }
    let has = |f: &str| seen.contains(&f);
    Ok(match cmd {
        "get" => Command::Get {
            refresh: has("--refresh"),
        },
        "fetch" => Command::Fetch { now: has("--now") },
        "status" => Command::Status,
        "install" => {
            if has("--timer") && has("--no-timer") {
                return Err("install: --timer and --no-timer exclude each other".into());
            }
            let timer = if has("--timer") {
                Some(true)
            } else if has("--no-timer") {
                Some(false)
            } else {
                None
            };
            Command::Install {
                timer,
                status: has("--status"),
            }
        }
        "uninstall" => Command::Uninstall {
            purge: has("--purge"),
        },
        _ => unreachable!("every command is matched above"),
    })
}

fn top_level(flag: &str) -> Command {
    if flag == "--version" {
        Command::Version
    } else {
        Command::Help
    }
}

/// Writes to stdout. A reader that closed the pipe is not an error (exit 0, no panic); any
/// other write error (a full disk) is said on stderr, exit 1. (A stdout already closed at
/// start is not seen: Rust's std treats it as a sink.)
fn out(text: &str) -> ExitCode {
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            err(&format!("claude-usage: cannot write output: {e}\n"));
            ExitCode::FAILURE
        }
    }
}

fn err(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

/// Unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn main() -> ExitCode {
    sys::harden();
    // A fixed line, never the panic's payload (it could hold a value).
    std::panic::set_hook(Box::new(|_| err("<3>claude-usage: internal error\n")));
    let raw: Vec<String> = match std::env::args_os()
        .skip(1)
        .map(|a| a.into_string())
        .collect()
    {
        Ok(v) => v,
        Err(_) => {
            err("claude-usage: arguments must be UTF-8\n");
            return ExitCode::from(2);
        }
    };
    let args: Vec<&str> = raw.iter().map(String::as_str).collect();
    match parse(&args) {
        Ok(Command::Help) => out(USAGE),
        Ok(Command::Version) => out(&format!("claude-usage {}\n", env!("CARGO_PKG_VERSION"))),
        Ok(Command::Get { refresh }) => ExitCode::from(get::run(refresh)),
        Ok(Command::Fetch { now }) => ExitCode::from(fetch::run(now)),
        Ok(Command::Status) => ExitCode::from(status::run()),
        Ok(cmd) => {
            err(&format!(
                "claude-usage: {}: not implemented yet\n",
                cmd.name()
            ));
            ExitCode::from(2)
        }
        Err(msg) => {
            err(&format!("claude-usage: {msg}\n{USAGE}"));
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_and_flags() {
        assert_eq!(parse(&["get"]), Ok(Command::Get { refresh: false }));
        assert_eq!(
            parse(&["get", "--refresh"]),
            Ok(Command::Get { refresh: true })
        );
        assert_eq!(parse(&["fetch", "--now"]), Ok(Command::Fetch { now: true }));
        assert_eq!(parse(&["status"]), Ok(Command::Status));
        assert_eq!(
            parse(&["install", "--no-timer", "--status"]),
            Ok(Command::Install {
                timer: Some(false),
                status: true
            })
        );
        assert_eq!(
            parse(&["install"]),
            Ok(Command::Install {
                timer: None,
                status: false
            })
        );
        assert_eq!(
            parse(&["uninstall", "--purge"]),
            Ok(Command::Uninstall { purge: true })
        );
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--version"]), Ok(Command::Version));
    }

    #[test]
    fn refusals() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["nope"]).is_err());
        assert!(parse(&["get", "--now"]).is_err());
        assert!(parse(&["get", "--refresh", "--refresh"]).is_err());
        assert!(parse(&["install", "--timer", "--no-timer"]).is_err());
        assert!(parse(&["status", "--purge"]).is_err());
        assert!(parse(&["--help", "get"]).is_err());
    }

    #[test]
    fn errors_name_the_real_problem() {
        assert_eq!(
            parse(&["nope", "--x", "--x"]),
            Err("unknown command: nope".into())
        );
        assert_eq!(
            parse(&["get", "--x"]),
            Err("get: unknown option: --x".into())
        );
    }

    #[test]
    fn help_after_a_command() {
        assert_eq!(parse(&["get", "--help"]), Ok(Command::Help));
        assert_eq!(parse(&["install", "--timer", "-h"]), Ok(Command::Help));
    }
}
