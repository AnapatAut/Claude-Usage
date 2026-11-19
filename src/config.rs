//! The optional config file `$HOME/.config/claude-usage/config`: lines `key = value`, `#` starts
//! a comment. Read only when `$HOME` is the user's home in the passwd file and the file is a
//! regular file of the user's, not writable by group or others, at most 64 KiB; otherwise
//! ignored, as if absent. v1 key: `curl = /absolute/path`.
//! The passwd rule: a project tool that sets `HOME` to a folder of a repository (while
//! `CLAUDE_CONFIG_DIR` still names the real credentials) must not choose the curl that gets the
//! token.

use crate::{paths, sys};
use std::io::Read;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    /// As written; checked when curl is chosen (a relative path is refused there).
    pub curl: Option<PathBuf>,
}

/// Whether a config file under `home` may be read: `home` is the passwd home of the effective
/// user (both canonical). No passwd entry: no.
pub fn honoured(home: &Path) -> bool {
    sys::passwd_home(sys::euid()).is_some_and(|p| paths::canonical(home) == paths::canonical(&p))
}

pub fn read(home: &Path) -> Config {
    if !honoured(home) {
        return Config::default();
    }
    let p = home.join(".config/claude-usage/config");
    let Ok(fd) = sys::open(&p, libc::O_RDONLY | libc::O_NONBLOCK) else {
        return Config::default();
    };
    let ok = sys::fstat(fd.as_fd()).is_ok_and(|st| {
        st.is_file()
            && st.uid == sys::euid()
            && st.perm() & 0o022 == 0
            && u64::try_from(st.size).is_ok_and(|n| n <= MAX_BYTES)
    });
    let mut text = String::new();
    if !ok
        || std::fs::File::from(fd)
            .take(MAX_BYTES + 1)
            .read_to_string(&mut text)
            .is_err()
    {
        return Config::default();
    }
    parse(&text)
}

fn parse(text: &str) -> Config {
    let mut c = Config::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "curl" {
            c.curl = Some(PathBuf::from(v.trim()));
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        assert_eq!(parse(""), Config::default());
        assert_eq!(
            parse("# c\n\n  curl =  /opt/x/curl  # trailing\nother = 1\nnonsense\n"),
            Config {
                curl: Some("/opt/x/curl".into())
            }
        );
        assert_eq!(
            parse("curl = /a\ncurl=/b\n"),
            Config {
                curl: Some("/b".into())
            }
        );
    }
}
