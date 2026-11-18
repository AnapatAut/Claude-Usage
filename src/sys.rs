//! Thin, checked wrappers around the system calls std does not offer: directory fds (openat,
//! mkdirat, renameat, unlinkat), flock, process hardening, close-on-exec of inherited fds.
//! Every fd this module opens is close-on-exec.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

fn cstring(s: &OsStr) -> io::Result<CString> {
    CString::new(s.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn check(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Runs a call again when a signal interrupted it.
fn retry(mut f: impl FnMut() -> libc::c_int) -> io::Result<libc::c_int> {
    loop {
        match check(f()) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => return r,
        }
    }
}

pub fn errno_is(e: &io::Error, code: libc::c_int) -> bool {
    e.raw_os_error() == Some(code)
}

/// The fields of `struct stat` the program uses.
pub struct Stat {
    pub dev: libc::dev_t,
    pub ino: libc::ino_t,
    pub mode: libc::mode_t,
    pub rdev: libc::dev_t,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    pub size: libc::off_t,
    pub mtime: i64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
}

impl Stat {
    fn new(s: &libc::stat) -> Stat {
        Stat {
            dev: s.st_dev,
            ino: s.st_ino,
            mode: s.st_mode,
            rdev: s.st_rdev,
            uid: s.st_uid,
            gid: s.st_gid,
            size: s.st_size,
            mtime: s.st_mtime,
            mtime_ns: i128::from(s.st_mtime) * 1_000_000_000 + i128::from(s.st_mtime_nsec),
            ctime_ns: i128::from(s.st_ctime) * 1_000_000_000 + i128::from(s.st_ctime_nsec),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn is_file(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
    }

    pub fn perm(&self) -> libc::mode_t {
        self.mode & 0o7777
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFLNK
    }
}

pub fn fstat(fd: BorrowedFd<'_>) -> io::Result<Stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a valid fd and a buffer of the right type; fstat fills it on success.
    check(unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) })?;
    // SAFETY: fstat succeeded, so the buffer is initialised.
    Ok(Stat::new(unsafe { st.assume_init_ref() }))
}

/// `stat`, following symlinks.
pub fn stat(path: &Path) -> io::Result<Stat> {
    let c = cstring(path.as_os_str())?;
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a NUL-terminated path and a buffer of the right type.
    check(unsafe { libc::stat(c.as_ptr(), st.as_mut_ptr()) })?;
    // SAFETY: stat succeeded.
    Ok(Stat::new(unsafe { st.assume_init_ref() }))
}

/// `fstatat` of a name in a folder, not following a symlink.
pub fn stat_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Stat> {
    let c = cstring(name)?;
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a valid folder fd, a NUL-terminated name, a buffer of the right type.
    check(unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    // SAFETY: fstatat succeeded.
    Ok(Stat::new(unsafe { st.assume_init_ref() }))
}

/// `open`, always close-on-exec.
pub fn open(path: &Path, flags: libc::c_int) -> io::Result<OwnedFd> {
    let c = cstring(path.as_os_str())?;
    // SAFETY: a NUL-terminated path; no O_CREAT, so no mode argument is read.
    let fd = retry(|| unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC) })?;
    // SAFETY: open returned a new fd that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `openat` of a name in a folder, always close-on-exec. `mode` is used only with O_CREAT.
pub fn open_at(
    dir: BorrowedFd<'_>,
    name: &str,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<OwnedFd> {
    let c = cstring(OsStr::new(name))?;
    // SAFETY: a valid folder fd and a NUL-terminated name; the mode is passed as the variadic
    // argument openat reads with O_CREAT.
    let fd = retry(|| unsafe {
        libc::openat(dir.as_raw_fd(), c.as_ptr(), flags | libc::O_CLOEXEC, mode)
    })?;
    // SAFETY: openat returned a new fd that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn mkdir_at(dir: BorrowedFd<'_>, name: &str, mode: libc::mode_t) -> io::Result<()> {
    let c = cstring(OsStr::new(name))?;
    // SAFETY: a valid folder fd and a NUL-terminated name.
    check(unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), mode) }).map(drop)
}

pub fn rename_at(dir: BorrowedFd<'_>, from: &str, to: &str) -> io::Result<()> {
    let (f, t) = (cstring(OsStr::new(from))?, cstring(OsStr::new(to))?);
    // SAFETY: a valid folder fd and NUL-terminated names.
    check(unsafe { libc::renameat(dir.as_raw_fd(), f.as_ptr(), dir.as_raw_fd(), t.as_ptr()) })
        .map(drop)
}

pub fn unlink_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<()> {
    let c = cstring(name)?;
    // SAFETY: a valid folder fd and a NUL-terminated name; flags 0 removes a file only.
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) }).map(drop)
}

/// Removes an empty folder in a folder.
pub fn rmdir_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<()> {
    let c = cstring(name)?;
    // SAFETY: a valid folder fd and a NUL-terminated name.
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) }).map(drop)
}

/// Makes a symlink `name` in a folder, pointing at `target`.
pub fn symlink_at(target: &Path, dir: BorrowedFd<'_>, name: &str) -> io::Result<()> {
    let (t, n) = (cstring(target.as_os_str())?, cstring(OsStr::new(name))?);
    // SAFETY: NUL-terminated strings and a valid folder fd.
    check(unsafe { libc::symlinkat(t.as_ptr(), dir.as_raw_fd(), n.as_ptr()) }).map(drop)
}

/// What a symlink in a folder points at.
pub fn readlink_at(dir: BorrowedFd<'_>, name: &str) -> io::Result<PathBuf> {
    let c = cstring(OsStr::new(name))?;
    let mut buf = vec![0u8; 4096];
    // SAFETY: a valid folder fd, a NUL-terminated name, and a buffer of the given length.
    let n = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            c.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    if n >= buf.len() {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    buf.truncate(n);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

pub fn fchmod(fd: BorrowedFd<'_>, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: a valid fd.
    check(unsafe { libc::fchmod(fd.as_raw_fd(), mode) }).map(drop)
}

pub fn fsync(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: a valid fd.
    retry(|| unsafe { libc::fsync(fd.as_raw_fd()) }).map(drop)
}

/// `flock`; with LOCK_NB, a held lock is an error of kind WouldBlock.
pub fn flock(fd: BorrowedFd<'_>, op: libc::c_int) -> io::Result<()> {
    // SAFETY: a valid fd.
    retry(|| unsafe { libc::flock(fd.as_raw_fd(), op) }).map(drop)
}

/// The names in a folder, `.` and `..` left out.
pub fn names(dir: BorrowedFd<'_>) -> io::Result<Vec<OsString>> {
    // A duplicate, so that closedir does not close the caller's fd.
    // SAFETY: a valid fd; F_DUPFD_CLOEXEC returns a new one.
    let fd = check(unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })?;
    // SAFETY: fd is a folder fd we own; on success the DIR stream owns it.
    let d = unsafe { libc::fdopendir(fd) };
    if d.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so fd is still ours to close.
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let mut out = Vec::new();
    // SAFETY: d is a valid DIR stream until closedir; each entry's name is NUL-terminated and
    // copied before the next readdir.
    unsafe {
        libc::rewinddir(d);
        loop {
            let e = libc::readdir(d);
            if e.is_null() {
                break;
            }
            let name = CStr::from_ptr((*e).d_name.as_ptr()).to_bytes();
            if name != b"." && name != b".." {
                out.push(OsString::from_vec(name.to_vec()));
            }
        }
        libc::closedir(d);
    }
    Ok(out)
}

pub fn euid() -> libc::uid_t {
    // SAFETY: no arguments, cannot fail.
    unsafe { libc::geteuid() }
}

pub fn ruid() -> libc::uid_t {
    // SAFETY: no arguments, cannot fail.
    unsafe { libc::getuid() }
}

pub fn egid() -> libc::gid_t {
    // SAFETY: no arguments, cannot fail.
    unsafe { libc::getegid() }
}

/// At start, for every command: files owner-only, no core dump, not dumpable (no same-user
/// ptrace or /proc/pid/mem; the environment in /proc hidden). curl inherits the core limit.
pub fn harden() {
    const Z: libc::c_ulong = 0;
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain system calls with valid arguments; failures leave the process as it was.
    unsafe {
        libc::umask(0o077);
        libc::prctl(libc::PR_SET_DUMPABLE, Z, Z, Z, Z);
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
    }
}

/// Ends the process after `secs` seconds: SIGALRM set to its default action (end the process)
/// and unblocked first, whatever the parent left (`trap '' ALRM`, a blocked mask).
pub fn deadline(secs: libc::c_uint) {
    // SAFETY: plain signal-state calls with valid arguments, made while single-threaded.
    unsafe {
        libc::signal(libc::SIGALRM, libc::SIG_DFL);
        let mut set = MaybeUninit::<libc::sigset_t>::zeroed();
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), libc::SIGALRM);
        libc::sigprocmask(libc::SIG_UNBLOCK, set.as_ptr(), std::ptr::null_mut());
        libc::alarm(secs);
    }
}

/// Whether stderr is /dev/null (a detached fetch): a refusal said there reaches nobody.
pub fn stderr_is_devnull() -> bool {
    // SAFETY: fd 2 is open for the life of the process (std reopens a closed one on
    // /dev/null at start); the borrow ends with this call.
    let fd = unsafe { BorrowedFd::borrow_raw(2) };
    let chr = |st: &Stat| st.mode & libc::S_IFMT == libc::S_IFCHR;
    match (fstat(fd), stat(Path::new("/dev/null"))) {
        (Ok(a), Ok(b)) => chr(&a) && chr(&b) && a.rdev == b.rdev,
        _ => false,
    }
}

/// Fills `buf` from the kernel's random source.
pub fn random(buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        // SAFETY: the pointer and length name the unfilled rest of buf.
        let n = unsafe { libc::getrandom(buf[done..].as_mut_ptr().cast(), buf.len() - done, 0) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
            continue;
        }
        done += n.unsigned_abs();
    }
    Ok(())
}

/// Marks every fd from `first` up close-on-exec. Meant for a `pre_exec` hook: only system calls,
/// no allocation. close_range (Linux 5.11+) as a raw syscall (musl has no wrapper), else each fd
/// up to the soft limit (at most 65536).
pub fn cloexec_from(first: libc::c_uint) {
    // SAFETY: close_range only changes fd flags; on an older kernel it fails with ENOSYS.
    let r = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if r == 0 {
        return;
    }
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid out-pointer.
    let max = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
        lim.rlim_cur.min(65536)
    } else {
        65536
    };
    let mut fd = libc::rlim_t::from(first);
    while fd < max {
        // SAFETY: fcntl on a number that may not be an open fd fails with EBADF, harmlessly.
        unsafe { libc::fcntl(fd as libc::c_int, libc::F_SETFD, libc::FD_CLOEXEC) };
        fd += 1;
    }
}

/// `setsid`, for a `pre_exec` hook.
pub fn setsid() {
    // SAFETY: always valid; fails only for a process group leader, which a fresh child is not.
    unsafe { libc::setsid() };
}

/// The home folder in the passwd entry of `uid`: None when there is no entry.
pub fn passwd_home(uid: libc::uid_t) -> Option<PathBuf> {
    passwd_entry(uid).map(|(_, home)| home)
}

/// The user name and home folder in the passwd entry of `uid`.
pub fn passwd_entry(uid: libc::uid_t) -> Option<(OsString, PathBuf)> {
    let mut buf = vec![0u8; 16384];
    let mut pw = MaybeUninit::<libc::passwd>::uninit();
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: valid buffers; getpwuid_r writes into pw and buf and sets res.
    let r = unsafe {
        libc::getpwuid_r(
            uid,
            pw.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut res,
        )
    };
    if r != 0 || res.is_null() {
        return None;
    }
    // SAFETY: res points at pw, filled; pw_name and pw_dir point into buf, NUL-terminated.
    let (name, dir) = unsafe {
        (
            CStr::from_ptr((*res).pw_name),
            CStr::from_ptr((*res).pw_dir),
        )
    };
    Some((
        OsStr::from_bytes(name.to_bytes()).to_owned(),
        PathBuf::from(OsStr::from_bytes(dir.to_bytes())),
    ))
}

/// Sends SIGKILL to a process group.
pub fn kill_group(pgid: libc::pid_t) {
    if pgid > 1 {
        // SAFETY: a negative pid names a process group.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

/// Sets O_NONBLOCK on an fd.
pub fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: a valid fd.
    let fl = check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })?;
    // SAFETY: a valid fd and flags.
    check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK) }).map(drop)
}
