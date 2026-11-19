//! The optional config file `$HOME/.config/claude-usage/config`: lines `key = value`, `#` starts
//! a comment. Read only when it is a regular file of the user's, not writable by group or
//! others, at most 64 KiB; otherwise ignored, as if absent. v1 key: `curl = /absolute/path`.

use crate::sys;
use std::io::Read;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    /// As written; checked when curl is chosen (a relative path is refused there).
    pub curl: Option<PathBuf>,
}

pub fn read(home: &Path) -> Config {
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
