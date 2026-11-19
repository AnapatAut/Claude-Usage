//! `.lock`: one fetch at a time per config folder. Whether a fetch holds it is always the
//! kernel's answer (`flock`), never the pid recorded inside, which is for `status` only.

use crate::sys;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

/// `.lock` for a fetch: created 0600, never through a symlink, a regular file of the user's.
pub fn open(dir: &OwnedFd) -> Option<File> {
    let fd = sys::open_at(
        dir.as_fd(),
        ".lock",
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
        0o600,
    )
    .ok()?;
    let st = sys::fstat(fd.as_fd()).ok()?;
    (st.is_file() && st.uid == sys::euid()).then(|| File::from(fd))
}

pub enum Taken {
    Yes,
    /// Another fetch still held it at the end of the wait.
    Busy,
    /// flock itself failed (a filesystem without locks): never read as "held".
    Failed,
}

/// Takes the lock exclusively, trying for up to `wait`.
pub fn acquire(f: &File, wait: Duration) -> Taken {
    let end = Instant::now() + wait;
    loop {
        match sys::flock(f.as_fd(), libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => return Taken::Yes,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= end {
                    return Taken::Busy;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return Taken::Failed,
        }
    }
}

/// Records the holder for `status`: pid and start time.
pub fn record(f: &File, pid: u32, since: i64) {
    let _ = f.set_len(0);
    let _ = f.write_all_at(format!("{pid} {since}\n").as_bytes(), 0);
}

pub enum Holder {
    /// No fetch holds it. The shared lock, when there is a `.lock`, is held until this is
    /// dropped: while it is, no fetch can start writing.
    Free(#[allow(dead_code)] Option<File>),
    Held,
    /// `.lock` is a symlink, not a regular file of the user's, or cannot be locked: a fetch
    /// would refuse, so the state counts as unusable.
    Bad,
}

/// The `LOCK_SH|LOCK_NB` test of `get` and `status`; creates nothing.
pub fn test(dir: &OwnedFd) -> Holder {
    let fd = match sys::open_at(
        dir.as_fd(),
        ".lock",
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    ) {
        Ok(fd) => fd,
        Err(e) if sys::errno_is(&e, libc::ENOENT) => return Holder::Free(None),
        Err(_) => return Holder::Bad,
    };
    if !sys::fstat(fd.as_fd()).is_ok_and(|st| st.is_file() && st.uid == sys::euid()) {
        return Holder::Bad;
    }
    match sys::flock(fd.as_fd(), libc::LOCK_SH | libc::LOCK_NB) {
        Ok(()) => Holder::Free(Some(File::from(fd))),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Holder::Held,
        Err(_) => Holder::Bad,
    }
}

/// The recorded holder (pid, since), for `status`.
pub fn recorded(dir: &OwnedFd) -> Option<(u32, i64)> {
    let fd = sys::open_at(
        dir.as_fd(),
        ".lock",
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )
    .ok()?;
    let mut s = String::new();
    File::from(fd).take(64).read_to_string(&mut s).ok()?;
    let (p, t) = s.trim().split_once(' ')?;
    Some((p.parse().ok()?, t.parse().ok()?))
}
