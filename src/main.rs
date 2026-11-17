//! claude-usage: one shared, cached fetcher for Claude usage limits.
//!
//! This is the command shape only; every command still answers "not implemented".

use std::io::Write;
use std::process::ExitCode;

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
    let mut seen: Vec<&str> = Vec::new();
    for &flag in rest {
        if seen.contains(&flag) {
            return Err(format!("{flag} given twice"));
        }
        seen.push(flag);
    }
    let allowed: &[&str] = match cmd {
        "get" => &["--refresh"],
        "fetch" => &["--now"],
        "install" => &["--timer", "--no-timer", "--status"],
        "uninstall" => &["--purge"],
        "status" | "--help" | "-h" | "--version" => &[],
        _ => return Err(format!("unknown command: {cmd}")),
    };
    if let Some(bad) = seen.iter().find(|f| !allowed.contains(f)) {
        return Err(format!("{cmd}: unknown option: {bad}"));
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
        "--version" => Command::Version,
        _ => Command::Help,
    })
}

/// Writes to stdout and ignores a write error (a reader that closed the pipe must not turn
/// into a panic).
fn out(text: &str) {
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
}

fn err(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

fn main() -> ExitCode {
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
        Ok(Command::Help) => {
            out(USAGE);
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            out(&format!("claude-usage {}\n", env!("CARGO_PKG_VERSION")));
            ExitCode::SUCCESS
        }
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
}
