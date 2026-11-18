//! `get --refresh`: starts one detached `fetch` and returns at once.

use crate::sys;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Spawns `exe fetch` (argv[0] = that absolute path) for `config_dir`: its own session,
/// stdin/stdout/stderr on /dev/null, working folder `/`, every inherited fd from 3 up
/// close-on-exec (a reader's pipe must reach EOF when the reader is done). Not waited for: the
/// nearest subreaper or init reaps it. One process, no thread.
pub fn spawn_fetch(exe: &Path, config_dir: &Path) -> bool {
    let mut cmd = Command::new(exe);
    cmd.arg("fetch")
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir("/");
    // SAFETY: the hook makes only system calls (no allocation, no locks).
    unsafe {
        cmd.pre_exec(|| {
            sys::setsid();
            sys::cloexec_from(3);
            Ok(())
        });
    }
    cmd.spawn().is_ok()
}
