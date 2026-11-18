//! Where things are: `$HOME`, the Claude config folder, the state key, the program's own path.

use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// Why no config folder could be named.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// `CLAUDE_CONFIG_DIR` is relative (`bad_config`).
    Relative,
    /// `CLAUDE_CONFIG_DIR` unset and no usable `$HOME`.
    NoHome,
}

/// `$HOME` when it is an absolute path. `XDG_*` are never used (a shell and the timer with
/// different values would split the state).
pub fn home() -> Option<PathBuf> {
    home_from(std::env::var_os("HOME"))
}

fn home_from(v: Option<OsString>) -> Option<PathBuf> {
    v.map(PathBuf::from).filter(|p| p.is_absolute())
}

/// The Claude config folder, canonical: `CLAUDE_CONFIG_DIR` (empty counts as unset), else
/// `~/.claude`.
pub fn config_dir(home: Option<&Path>) -> Result<PathBuf, ConfigError> {
    config_dir_from(std::env::var_os("CLAUDE_CONFIG_DIR"), home)
}

fn config_dir_from(v: Option<OsString>, home: Option<&Path>) -> Result<PathBuf, ConfigError> {
    match v.filter(|v| !v.is_empty()) {
        Some(v) => {
            let p = PathBuf::from(v);
            if !p.is_absolute() {
                return Err(ConfigError::Relative);
            }
            Ok(canonical(&p))
        }
        None => home
            .map(|h| canonical(&h.join(".claude")))
            .ok_or(ConfigError::NoHome),
    }
}

/// `realpath`; for a folder that does not exist (yet), the absolute path with `.` and `..`
/// removed.
pub fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| lexical(p))
}

fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(D[usize::from(b >> 4)]));
        s.push(char::from(D[usize::from(b & 15)]));
    }
    s
}

/// The state folder's name for a config folder: the first 16 hex characters of the sha256 of
/// its canonical path. No path appears in folder names.
pub fn key(config_dir: &Path) -> String {
    let mut h = hex(&Sha256::digest(config_dir.as_os_str().as_bytes()));
    h.truncate(16);
    h
}

/// This program's own file, read from /proc/self/exe.
pub enum Exe {
    Path(PathBuf),
    /// Removed or replaced since it started (an uninstall or an upgrade).
    Deleted,
    /// /proc unreadable.
    Unknown,
}

pub fn exe() -> Exe {
    match std::fs::read_link("/proc/self/exe") {
        Ok(p) if p.as_os_str().as_bytes().ends_with(b" (deleted)") => Exe::Deleted,
        Ok(p) if p.is_absolute() => Exe::Path(p),
        _ => Exe::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_sha256_prefix() {
        // printf '%s' /nonexistent-u/.claude | sha256sum | cut -c1-16
        assert_eq!(key(Path::new("/nonexistent-u/.claude")), "89e7f6dfa1fa291a");
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn config_dir_rules() {
        let home = Path::new("/nonexistent-home");
        assert_eq!(
            config_dir_from(None, Some(home)),
            Ok(PathBuf::from("/nonexistent-home/.claude"))
        );
        assert_eq!(
            config_dir_from(Some("".into()), Some(home)),
            Ok(PathBuf::from("/nonexistent-home/.claude"))
        );
        assert_eq!(
            config_dir_from(Some("rel/x".into()), Some(home)),
            Err(ConfigError::Relative)
        );
        assert_eq!(config_dir_from(None, None), Err(ConfigError::NoHome));
        assert_eq!(
            config_dir_from(Some("/nonexistent-x/./a/../b/".into()), None),
            Ok(PathBuf::from("/nonexistent-x/b"))
        );
    }

    #[test]
    fn home_must_be_absolute() {
        assert_eq!(home_from(Some("/h".into())), Some(PathBuf::from("/h")));
        assert_eq!(home_from(Some("h".into())), None);
        assert_eq!(home_from(Some("".into())), None);
        assert_eq!(home_from(None), None);
    }

    #[test]
    fn lexical_never_climbs_above_root() {
        assert_eq!(lexical(Path::new("/../../a/./b/..")), PathBuf::from("/a"));
    }
}
