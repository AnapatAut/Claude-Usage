//! `install [--timer|--no-timer]`, `install --status` and `uninstall [--purge]` (design
//! section 8), for the user who runs it, never as root.
//!
//! dpkg style, no rollback: the manifest (`~/.local/share/claude-usage/manifest`) is written
//! first, marked pending, with every file and its sha256; each file is then written atomically
//! (a temp file in the same folder, fsync, rename, fsync of the folder); the manifest is
//! rewritten without the mark last. A run that stops part-way leaves the mark: running the same
//! command again finishes it. One run at a time (`flock` on the manifest's folder).
//!
//! A file is claude-usage's when it holds what this run writes, what the manifest records, or,
//! while a run is pending, what was there before it. Anything else at one of its paths (another
//! program's file, a file changed since the install, a symlink) is refused before anything is
//! written, and kept by uninstall. The manifest names its files by role, never by path.

use crate::curl;
use crate::lock::{self, Taken};
use crate::paths;
use crate::sys;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN_DIR: &str = ".local/bin";
const UNIT_DIR: &str = ".config/systemd/user";
const SHARE_DIR: &str = ".local/share";
const STATE_DIR: &str = ".local/state";
const NAME: &str = "claude-usage";
const MANIFEST: &str = "manifest";
const WANTS: &str = "timers.target.wants";
const TIMER_UNIT: &str = "claude-usage.timer";
const SERVICE_UNIT: &str = "claude-usage.service";
const SERVICE_TEXT: &str = include_str!("../systemd/claude-usage.service");
const TIMER_TEXT: &str = include_str!("../systemd/claude-usage.timer");
const HEADER: &str = "claude-usage manifest 1";
/// Larger than any build of this program; a larger file at its path is not its own.
const MAX_FILE: u64 = 64 << 20;
const MAX_MANIFEST: u64 = 4096;
/// How long uninstall waits for running fetches to let go of their state folders.
const LOCK_WAIT: Duration = Duration::from_secs(15);
const SYSTEMCTL: [&str; 2] = ["/usr/bin/systemctl", "/bin/systemctl"];
const SYSTEMCTL_WAIT: Duration = Duration::from_secs(60);
const NO_SYSTEMCTL: &str = "no systemctl on this machine";
const STATUSLINE_NOTE: &str = "note: while Claude-Statusline v1.3.x (before the release that uses claude-usage) runs, \
the usage endpoint gets two calls per 30 s while a Claude Code session is open";

// ------------------------------------------------------------------ the files

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Part {
    Bin,
    Service,
    Timer,
}

impl Part {
    const ALL: [Part; 3] = [Part::Bin, Part::Service, Part::Timer];

    fn word(self) -> &'static str {
        match self {
            Part::Bin => "bin",
            Part::Service => "service",
            Part::Timer => "timer",
        }
    }

    fn from_word(w: &str) -> Option<Part> {
        Part::ALL.into_iter().find(|p| p.word() == w)
    }

    fn file(self) -> &'static str {
        match self {
            Part::Bin => NAME,
            Part::Service => SERVICE_UNIT,
            Part::Timer => TIMER_UNIT,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Part::Bin => BIN_DIR,
            Part::Service | Part::Timer => UNIT_DIR,
        }
    }

    fn mode(self) -> libc::mode_t {
        match self {
            Part::Bin => 0o700,
            Part::Service | Part::Timer => 0o600,
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    /// `~/…`, for messages.
    fn shown(self) -> String {
        format!("~/{}/{}", self.dir(), self.file())
    }
}

// ------------------------------------------------------------------ the manifest

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pending {
    Install,
    Uninstall,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Record {
    sha: String,
    /// While a run is pending: the file's hash before it (None: there was no file).
    was: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Manifest {
    version: String,
    timer: bool,
    pending: Option<Pending>,
    files: [Option<Record>; 3],
}

fn is_sha(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_version(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
}

impl Manifest {
    fn render(&self) -> String {
        let mut s = format!(
            "{HEADER}\nversion {}\ntimer {}\n",
            self.version,
            if self.timer { "on" } else { "off" }
        );
        match self.pending {
            Some(Pending::Install) => s.push_str("pending install\n"),
            Some(Pending::Uninstall) => s.push_str("pending uninstall\n"),
            None => {}
        }
        for p in Part::ALL {
            if let Some(r) = &self.files[p.index()] {
                s.push_str(&format!(
                    "file {} {} {}\n",
                    p.word(),
                    r.sha,
                    r.was.as_deref().unwrap_or("-")
                ));
            }
        }
        s
    }

    /// Strict: the header, `version` and `timer` once each, at most one `pending` and one
    /// `file` line per part; nothing else.
    fn parse(text: &str) -> Option<Manifest> {
        let mut lines = text.lines();
        if lines.next()? != HEADER {
            return None;
        }
        let (mut version, mut timer, mut pending) = (None, None, None);
        let mut files: [Option<Record>; 3] = [None, None, None];
        for line in lines {
            let f: Vec<&str> = line.split(' ').collect();
            match f.as_slice() {
                ["version", v] if version.is_none() && is_version(v) => version = Some(*v),
                ["timer", "on"] if timer.is_none() => timer = Some(true),
                ["timer", "off"] if timer.is_none() => timer = Some(false),
                ["pending", "install"] if pending.is_none() => pending = Some(Pending::Install),
                ["pending", "uninstall"] if pending.is_none() => pending = Some(Pending::Uninstall),
                ["file", part, sha, was] => {
                    let p = Part::from_word(part)?;
                    if files[p.index()].is_some() || !is_sha(sha) || !(*was == "-" || is_sha(was)) {
                        return None;
                    }
                    files[p.index()] = Some(Record {
                        sha: (*sha).to_owned(),
                        was: (*was != "-").then(|| (*was).to_owned()),
                    });
                }
                _ => return None,
            }
        }
        // `was` counts only while a run is pending: a finished run clears it.
        if pending.is_none() && files.iter().flatten().any(|r| r.was.is_some()) {
            return None;
        }
        Some(Manifest {
            version: version?.to_owned(),
            timer: timer?,
            pending,
            files,
        })
    }

    /// Whether `sha` is a hash this manifest counts as claude-usage's for `part`.
    fn owns(&self, part: Part, sha: &str) -> bool {
        self.files[part.index()]
            .as_ref()
            .is_some_and(|r| r.sha == sha || r.was.as_deref() == Some(sha))
    }
}

// ------------------------------------------------------------------ files and folders

fn sha_hex(bytes: &[u8]) -> String {
    paths::hex(&Sha256::digest(bytes))
}

/// What is at one of the paths.
#[derive(Debug, PartialEq, Eq)]
enum Current {
    Absent,
    Hash(String),
    /// A symlink, not a regular file, not the user's, too large or unreadable.
    Bad(&'static str),
}

fn current(dir: &OwnedFd, name: &str) -> Current {
    let st = match sys::stat_at(dir.as_fd(), OsStr::new(name)) {
        Ok(st) => st,
        Err(e) if e.kind() == ErrorKind::NotFound => return Current::Absent,
        Err(_) => return Current::Bad("cannot be read"),
    };
    if st.is_symlink() {
        return Current::Bad("is a symbolic link");
    }
    if !st.is_file() {
        return Current::Bad("is not a regular file");
    }
    if st.uid != sys::euid() {
        return Current::Bad("is not yours");
    }
    let Ok(fd) = sys::open_at(
        dir.as_fd(),
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    ) else {
        return Current::Bad("cannot be read");
    };
    match sys::fstat(fd.as_fd()) {
        Ok(s) if s.is_file() && s.uid == sys::euid() => {}
        _ => return Current::Bad("changed while it was read"),
    }
    let mut h = Sha256::new();
    let mut f = File::from(fd).take(MAX_FILE + 1);
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                if total > MAX_FILE {
                    return Current::Bad("is too large to be claude-usage's");
                }
                h.update(&buf[..n]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return Current::Bad("cannot be read"),
        }
    }
    Current::Hash(paths::hex(&h.finalize()))
}

/// The program's own bytes (the running file, also when it was replaced since).
fn own_bytes() -> Option<Vec<u8>> {
    let f = File::open("/proc/self/exe").ok()?;
    let mut v = Vec::new();
    f.take(MAX_FILE + 1).read_to_end(&mut v).ok()?;
    (v.len() as u64 <= MAX_FILE && !v.is_empty()).then_some(v)
}

/// A folder of the user's that nobody else can write, reached by path: symlinks in it are
/// followed (dotfiles managers link `~/.config`). Group-writable only for the user's private
/// group (the effective group, named as the user, with no members listed) and without an ACL
/// (whose mask the group bits then are).
fn open_dir(home: &Path, rel: &str, create: bool) -> Result<Option<OwnedFd>, String> {
    let path = home.join(rel);
    if create
        && std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .is_err()
    {
        return Err(format!("~/{rel} cannot be created"));
    }
    let fd = match sys::open(&path, libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(fd) => fd,
        Err(e) if e.kind() == ErrorKind::NotFound && !create => return Ok(None),
        Err(_) => return Err(format!("~/{rel} is not a folder that can be opened")),
    };
    let st = sys::fstat(fd.as_fd()).map_err(|_| format!("~/{rel} cannot be checked"))?;
    let others_write = st.perm() & 0o002 != 0;
    let group_write = st.perm() & 0o020 != 0
        && (st.gid != sys::egid() || !private_group(st.gid) || sys::has_access_acl(fd.as_fd()));
    if !st.is_dir() || st.uid != sys::euid() || others_write || group_write {
        return Err(format!(
            "~/{rel} is not a folder of yours that only you can write"
        ));
    }
    Ok(Some(fd))
}

/// The user's own group: named as the user in the passwd entry, nobody else listed in it.
fn private_group(gid: libc::gid_t) -> bool {
    match (sys::group_entry(gid), sys::passwd_entry(sys::euid())) {
        (Some((group, members)), Some((user, _))) => !members && group == user,
        _ => false,
    }
}

/// `~/.local/share/claude-usage/`: never a symlink, the user's, mode 700.
fn open_share(home: &Path, create: bool) -> Result<Option<OwnedFd>, String> {
    let Some(parent) = open_dir(home, SHARE_DIR, create)? else {
        return Ok(None);
    };
    if create {
        match sys::mkdir_at(parent.as_fd(), NAME, 0o700) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(_) => return Err(format!("~/{SHARE_DIR}/{NAME} cannot be created")),
        }
    }
    let fd = match sys::open_at(
        parent.as_fd(),
        NAME,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    ) {
        Ok(fd) => fd,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(format!(
                "~/{SHARE_DIR}/{NAME} is not a folder (a symbolic link?)"
            ));
        }
    };
    let st =
        sys::fstat(fd.as_fd()).map_err(|_| format!("~/{SHARE_DIR}/{NAME} cannot be checked"))?;
    if !st.is_dir() || st.uid != sys::euid() {
        return Err(format!("~/{SHARE_DIR}/{NAME} is not a folder of yours"));
    }
    if st.perm() != 0o700 && create {
        sys::fchmod(fd.as_fd(), 0o700)
            .map_err(|_| format!("~/{SHARE_DIR}/{NAME}: chmod failed"))?;
    }
    Ok(Some(fd))
}

/// The manifest: Ok(None) when there is none, Err when it is not one this program wrote.
fn read_manifest(share: &OwnedFd) -> Result<Option<Manifest>, String> {
    let bad = || {
        format!(
            "~/{SHARE_DIR}/{NAME}/{MANIFEST} is not one claude-usage wrote: left as it is (remove it to start over)"
        )
    };
    match current(share, MANIFEST) {
        Current::Absent => return Ok(None),
        Current::Bad(_) => return Err(bad()),
        Current::Hash(_) => {}
    }
    let fd = sys::open_at(
        share.as_fd(),
        MANIFEST,
        libc::O_RDONLY | libc::O_NOFOLLOW,
        0,
    )
    .map_err(|_| bad())?;
    let mut text = String::new();
    File::from(fd)
        .take(MAX_MANIFEST)
        .read_to_string(&mut text)
        .map_err(|_| bad())?;
    Manifest::parse(&text).map(Some).ok_or_else(bad)
}

fn temp_prefix(name: &str) -> String {
    format!(".{name}.cu-")
}

/// Writes `name` in `dir` atomically with `mode`.
fn write_atomic(
    dir: &OwnedFd,
    name: &str,
    bytes: &[u8],
    mode: libc::mode_t,
) -> std::io::Result<()> {
    const A: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut made = None;
    for _ in 0..16 {
        let mut r = [0u8; 6];
        sys::random(&mut r)?;
        let mut t = temp_prefix(name);
        t.extend(r.iter().map(|b| char::from(A[usize::from(*b) % A.len()])));
        match sys::open_at(
            dir.as_fd(),
            &t,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        ) {
            Ok(fd) => {
                made = Some((t, fd));
                break;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let Some((temp, fd)) = made else {
        return Err(ErrorKind::AlreadyExists.into());
    };
    let mut f = File::from(fd);
    let r = sys::fchmod(f.as_fd(), mode)
        .and_then(|()| f.write_all(bytes))
        .and_then(|()| f.sync_all())
        .and_then(|()| sys::rename_at(dir.as_fd(), &temp, name));
    if r.is_err() {
        let _ = sys::unlink_at(dir.as_fd(), OsStr::new(&temp));
        return r;
    }
    let _ = sys::fsync(dir.as_fd());
    Ok(())
}

/// Removes temp files a killed run left for `names` (only one run holds the lock, so any is
/// stale).
fn sweep(dir: &OwnedFd, names: &[&str]) {
    let Ok(list) = sys::names(dir.as_fd()) else {
        return;
    };
    for n in list {
        let b = n.as_bytes();
        let ours = names.iter().any(|name| {
            let p = temp_prefix(name);
            b.len() == p.len() + 6 && b.starts_with(p.as_bytes())
        });
        if ours
            && let Ok(st) = sys::stat_at(dir.as_fd(), &n)
            && st.is_file()
            && st.uid == sys::euid()
        {
            let _ = sys::unlink_at(dir.as_fd(), &n);
        }
    }
}

/// Gives a file of the user's (never through a symlink) its mode.
fn fix_mode(dir: &OwnedFd, name: &str, mode: libc::mode_t) -> bool {
    let Ok(fd) = sys::open_at(
        dir.as_fd(),
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    ) else {
        return false;
    };
    match sys::fstat(fd.as_fd()) {
        Ok(st) if st.is_file() && st.uid == sys::euid() => {
            st.perm() == mode || sys::fchmod(fd.as_fd(), mode).is_ok()
        }
        _ => false,
    }
}

fn remove(dir: &OwnedFd, name: &str) -> bool {
    match sys::unlink_at(dir.as_fd(), OsStr::new(name)) {
        Ok(()) => {
            let _ = sys::fsync(dir.as_fd());
            true
        }
        Err(e) => e.kind() == ErrorKind::NotFound,
    }
}

// ------------------------------------------------------------------ the timer's link

#[derive(Debug, PartialEq, Eq)]
enum Link {
    Absent,
    /// A symlink to this unit folder's `claude-usage.timer`: the timer is enabled.
    Ours,
    /// Something else there: left alone.
    Other,
}

fn wants_dir(units: &OwnedFd) -> Option<OwnedFd> {
    sys::open_at(
        units.as_fd(),
        WANTS,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )
    .ok()
}

fn link_state(home: &Path, units: &OwnedFd) -> Link {
    let Some(wants) = wants_dir(units) else {
        return Link::Absent;
    };
    match sys::stat_at(wants.as_fd(), OsStr::new(TIMER_UNIT)) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Link::Absent,
        Ok(st) if st.is_symlink() => {}
        _ => return Link::Other,
    }
    let Ok(target) = sys::readlink_at(wants.as_fd(), TIMER_UNIT) else {
        return Link::Other;
    };
    let target = if target.is_absolute() {
        target
    } else {
        home.join(UNIT_DIR).join(WANTS).join(target)
    };
    // The link's folder must be this unit folder (followed through any symlink): compared by
    // device and inode, so a link to a copy elsewhere is not counted.
    let same_dir = target
        .parent()
        .and_then(|p| sys::stat(p).ok())
        .zip(sys::fstat(units.as_fd()).ok());
    match same_dir {
        Some((a, b))
            if a.dev == b.dev
                && a.ino == b.ino
                && target.file_name() == Some(OsStr::new(TIMER_UNIT)) =>
        {
            Link::Ours
        }
        _ => Link::Other,
    }
}

/// Enables the timer without the user manager: the link `systemctl enable` makes.
fn link_timer(home: &Path, units: &OwnedFd) -> bool {
    match link_state(home, units) {
        Link::Ours => return true,
        Link::Other => return false,
        Link::Absent => {}
    }
    let _ = sys::mkdir_at(units.as_fd(), WANTS, 0o700);
    let Some(wants) = wants_dir(units) else {
        return false;
    };
    let ok = sys::symlink_at(
        &home.join(UNIT_DIR).join(TIMER_UNIT),
        wants.as_fd(),
        TIMER_UNIT,
    )
    .is_ok();
    let _ = sys::fsync(wants.as_fd());
    ok && link_state(home, units) == Link::Ours
}

fn unlink_timer(home: &Path, units: &OwnedFd) -> bool {
    match link_state(home, units) {
        Link::Absent => true,
        Link::Other => false,
        Link::Ours => wants_dir(units).is_some_and(|w| remove(&w, TIMER_UNIT)),
    }
}

// ------------------------------------------------------------------ systemctl --user

struct Systemctl {
    path: PathBuf,
    runtime: PathBuf,
}

/// The system's systemctl (root's, never from PATH) and the user manager's runtime folder:
/// `XDG_RUNTIME_DIR`, else `/run/user/<uid>`, a folder of the user's with the manager's socket.
fn systemctl() -> Result<Systemctl, &'static str> {
    let path = SYSTEMCTL
        .iter()
        .find_map(|p| curl::trusted(Path::new(p), false))
        .ok_or(NO_SYSTEMCTL)?;
    let euid = sys::euid();
    let mine =
        |p: &Path| p.is_absolute() && sys::stat(p).is_ok_and(|st| st.is_dir() && st.uid == euid);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| mine(p))
        .or_else(|| Some(PathBuf::from(format!("/run/user/{euid}"))).filter(|p| mine(p)))
        .ok_or("no user session (XDG_RUNTIME_DIR)")?;
    if !runtime.join("systemd/private").exists() {
        return Err("your systemd user manager is not running");
    }
    Ok(Systemctl { path, runtime })
}

impl Systemctl {
    /// `systemctl --user ARGS`: an empty environment but the runtime folder, stdin
    /// /dev/null, its output (both streams, at most 16 KiB) for messages, 60 s at most.
    fn run(&self, args: &[&str]) -> (bool, String) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: a valid array of two ints.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return (false, "cannot make a pipe".into());
        }
        // SAFETY: pipe2 returned two new fds that nothing else owns.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let Ok(w2) = w.try_clone() else {
            return (false, "cannot make a pipe".into());
        };
        let mut cmd = Command::new(&self.path);
        cmd.arg("--user")
            .args(args)
            .env_clear()
            .env("LANG", "C")
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("SYSTEMD_PAGER", "")
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::from(w))
            .stderr(Stdio::from(w2))
            .process_group(0);
        // SAFETY: the hook makes only system calls (no allocation, no locks).
        unsafe {
            cmd.pre_exec(|| {
                sys::cloexec_from(3);
                Ok(())
            });
        }
        let Ok(mut child) = cmd.spawn() else {
            return (false, "systemctl did not start".into());
        };
        drop(cmd); // the parent's write ends, so the read ends at EOF
        let pid = libc::pid_t::try_from(child.id()).unwrap_or(0);
        let end = Instant::now() + SYSTEMCTL_WAIT;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        let mut f = File::from(r);
        let timed_out = loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break true;
            }
            let mut p = libc::pollfd {
                fd: f.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = libc::c_int::try_from(left.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
            // SAFETY: one valid pollfd.
            if unsafe { libc::poll(&mut p, 1, ms) } < 0 {
                if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
                    continue;
                }
                break true;
            }
            if p.revents == 0 {
                continue;
            }
            match f.read(&mut buf) {
                Ok(0) => break false,
                Ok(n) => {
                    if out.len() < 16384 {
                        out.extend_from_slice(&buf[..n.min(16384 - out.len())]);
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break false,
            }
        };
        if timed_out {
            sys::kill_group(pid);
            let _ = child.wait();
            return (false, "systemctl did not answer within 60 s".into());
        }
        let ok = child.wait().is_ok_and(|s| s.success());
        let text = String::from_utf8_lossy(&out);
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        (ok, line.chars().take(300).collect())
    }
}

// ------------------------------------------------------------------ who may run it

/// The design's rule for every command that writes: not root, not setuid, `$HOME` the user's.
fn who_may_run() -> Result<PathBuf, String> {
    let euid = sys::euid();
    if euid == 0 {
        return Err("running as root: run it as the user who uses Claude Code".into());
    }
    if sys::ruid() != euid {
        return Err("the real and effective user differ".into());
    }
    let home = paths::home().ok_or("HOME is not set or not absolute")?;
    if !sys::stat(&home).is_ok_and(|st| st.is_dir() && st.uid == euid) {
        return Err("HOME is not a folder of yours".into());
    }
    Ok(home)
}

fn say(line: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(line.as_bytes());
    let _ = o.write_all(b"\n");
    let _ = o.flush();
}

fn warn(line: &str) {
    let _ = std::io::stderr()
        .lock()
        .write_all(format!("claude-usage: {line}\n").as_bytes());
}

fn refuse(cmd: &str, why: &str) -> u8 {
    warn(&format!("{cmd} refused: {why}"));
    1
}

fn stopped(cmd: &str, why: &str) -> u8 {
    warn(&format!(
        "{cmd} stopped part-way: {why}. Run claude-usage {cmd} again to finish"
    ));
    3
}

/// Facts about the user's setup worth saying after install and in `--status`.
fn notes(home: &Path) -> Vec<String> {
    let mut v = Vec::new();
    let euid = sys::euid();
    match sys::passwd_entry(euid) {
        None => v.push(format!(
            "uid {euid} has no passwd entry: the timer's fetches may use another home"
        )),
        Some((name, phome)) => {
            if paths::canonical(home) != paths::canonical(&phome) {
                v.push(format!(
                    "HOME ({}) is not your passwd home ({}): the timer uses the passwd home, so the two keep separate state",
                    home.display(),
                    phome.display()
                ));
            }
            if !Path::new("/var/lib/systemd/linger").join(&name).exists() {
                v.push(
                    "linger is off: the timer runs only while you are logged in \
                     (loginctl enable-linger turns it on; tools that call get --refresh still start fetches)"
                        .into(),
                );
            }
        }
    }
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty()) {
        v.push(
            "CLAUDE_CONFIG_DIR is set: the timer's fetches use ~/.claude unless the user manager \
             has it too (systemctl --user set-environment CLAUDE_CONFIG_DIR=…, or a drop-in)"
                .into(),
        );
    }
    v
}

// ------------------------------------------------------------------ install

struct Dirs {
    bin: OwnedFd,
    units: OwnedFd,
}

impl Dirs {
    fn of(&self, p: Part) -> &OwnedFd {
        match p {
            Part::Bin => &self.bin,
            Part::Service | Part::Timer => &self.units,
        }
    }
}

/// What install finds at its paths, or why it refuses. A folder that is None is not there yet,
/// and holds nothing to refuse.
fn examine(
    home: &Path,
    old: Option<&Manifest>,
    bin: Option<&OwnedFd>,
    units: Option<&OwnedFd>,
    new: &[String; 3],
) -> Result<Vec<Current>, String> {
    if old.is_some_and(|m| m.pending == Some(Pending::Uninstall)) {
        return Err("an uninstall is unfinished: run claude-usage uninstall again first".into());
    }
    let mut problems = Vec::new();
    let mut now = Vec::new();
    for p in Part::ALL {
        let dir = match p {
            Part::Bin => bin,
            Part::Service | Part::Timer => units,
        };
        let cur = dir.map_or(Current::Absent, |d| current(d, p.file()));
        match &cur {
            Current::Absent => {}
            Current::Bad(why) => problems.push(format!("{} {why}", p.shown())),
            Current::Hash(h) if *h == new[p.index()] || old.is_some_and(|m| m.owns(p, h)) => {}
            Current::Hash(_) => {
                problems.push(if old.is_some_and(|m| m.files[p.index()].is_some()) {
                    format!(
                        "{} was changed since the install{}",
                        p.shown(),
                        dropin_hint(p)
                    )
                } else {
                    format!("{} is there and not claude-usage's", p.shown())
                })
            }
        }
        now.push(cur);
    }
    if units.is_some_and(|u| link_state(home, u) == Link::Other) {
        problems.push(format!(
            "~/{UNIT_DIR}/{WANTS}/{TIMER_UNIT} is there and not the timer's link"
        ));
    }
    if problems.is_empty() {
        Ok(now)
    } else {
        Err(format!("left as it is: {}", problems.join("; ")))
    }
}

/// The manifest's folder if it is there, locked, and the manifest in it.
fn look_locked(home: &Path) -> Result<(Option<OwnedFd>, Option<Manifest>), String> {
    let Some(share) = open_share(home, false)? else {
        return Ok((None, None));
    };
    if sys::flock(share.as_fd(), libc::LOCK_EX | libc::LOCK_NB).is_err() {
        return Err("another install or uninstall is running".into());
    }
    let old = read_manifest(&share)?;
    Ok((Some(share), old))
}

/// Makes `~/rel` a level at a time (mode 700), adding each folder it made to `made`.
fn make_dirs(home: &Path, rel: &str, made: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut p = home.to_path_buf();
    for c in Path::new(rel).components() {
        p.push(c);
        match std::fs::DirBuilder::new().mode(0o700).create(&p) {
            Ok(()) => made.push(p.clone()),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(_) => return Err(format!("~/{rel} cannot be created")),
        }
    }
    Ok(())
}

/// Removes the folders this run made, the deepest first, each only while empty.
fn unmake(made: &[PathBuf]) {
    for d in made.iter().rev() {
        let _ = std::fs::remove_dir(d);
    }
}

pub fn install(timer_flag: Option<bool>) -> u8 {
    const CMD: &str = "install";
    let home = match who_may_run() {
        Ok(h) => h,
        Err(m) => return refuse(CMD, &m),
    };
    // The user manager reads units from, and the service runs the program in (%h), the passwd
    // home: files placed under another HOME would never run, or leave another program to run.
    if let Some((_, phome)) = sys::passwd_entry(sys::euid())
        && paths::canonical(&home) != paths::canonical(&phome)
    {
        return refuse(
            CMD,
            &format!(
                "HOME ({}) is not your home in the passwd file ({}), which the timer uses: run it with HOME={}",
                home.display(),
                phome.display(),
                phome.display()
            ),
        );
    }
    let Some(bin) = own_bytes() else {
        return refuse(CMD, "cannot read its own program file");
    };
    let content = |p: Part| -> &[u8] {
        match p {
            Part::Bin => &bin,
            Part::Service => SERVICE_TEXT.as_bytes(),
            Part::Timer => TIMER_TEXT.as_bytes(),
        }
    };
    let new = Part::ALL.map(|p| sha_hex(content(p)));

    // A first look that makes nothing, so a refused install leaves no folder behind. The lock
    // is let go before the folders are made, so everything is checked again below, under it.
    {
        let (_share, old) = match look_locked(&home) {
            Ok(x) => x,
            Err(m) => return refuse(CMD, &m),
        };
        let (b, u) = match (
            open_dir(&home, BIN_DIR, false),
            open_dir(&home, UNIT_DIR, false),
        ) {
            (Ok(b), Ok(u)) => (b, u),
            (Err(m), _) | (_, Err(m)) => return refuse(CMD, &m),
        };
        if let Err(m) = examine(&home, old.as_ref(), b.as_ref(), u.as_ref(), &new) {
            return refuse(CMD, &m);
        }
    }

    // The folders, each level recorded when this run made it: a refusal from here on removes
    // them again (rmdir: only while empty), except when another run holds the lock.
    let mut made = Vec::new();
    if let Err(m) = make_dirs(&home, &format!("{SHARE_DIR}/{NAME}"), &mut made) {
        unmake(&made);
        return refuse(CMD, &m);
    }
    let share = match open_share(&home, true) {
        Ok(Some(s)) => s,
        Ok(None) => {
            unmake(&made);
            return refuse(CMD, "the manifest's folder cannot be made");
        }
        Err(m) => {
            unmake(&made);
            return refuse(CMD, &m);
        }
    };
    if sys::flock(share.as_fd(), libc::LOCK_EX | libc::LOCK_NB).is_err() {
        return refuse(CMD, "another install or uninstall is running");
    }
    let made_more =
        make_dirs(&home, BIN_DIR, &mut made).and_then(|()| make_dirs(&home, UNIT_DIR, &mut made));
    let back_out = |m: &str| {
        unmake(&made);
        refuse(CMD, m)
    };
    if let Err(m) = made_more {
        return back_out(&m);
    }
    let old = match read_manifest(&share) {
        Ok(m) => m,
        Err(m) => return back_out(&m),
    };
    let dirs = match (
        open_dir(&home, BIN_DIR, false),
        open_dir(&home, UNIT_DIR, false),
    ) {
        (Ok(Some(bin)), Ok(Some(units))) => Dirs { bin, units },
        (Err(m), _) | (_, Err(m)) => return back_out(&m),
        _ => return back_out("a folder cannot be made"),
    };
    sweep(&dirs.bin, &[NAME]);
    sweep(&dirs.units, &[SERVICE_UNIT, TIMER_UNIT]);
    sweep(&share, &[MANIFEST]);

    // Everything is checked before anything is written.
    let now = match examine(
        &home,
        old.as_ref(),
        Some(&dirs.bin),
        Some(&dirs.units),
        &new,
    ) {
        Ok(now) => now,
        Err(m) => return back_out(&m),
    };
    let mut wrote = 0;
    let timer = timer_flag.or(old.as_ref().map(|m| m.timer)).unwrap_or(true);
    let mut man = Manifest {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        timer,
        pending: Some(Pending::Install),
        files: [None, None, None],
    };
    for p in Part::ALL {
        let was = match &now[p.index()] {
            Current::Hash(h) => Some(h.clone()),
            _ => None,
        };
        man.files[p.index()] = Some(Record {
            sha: new[p.index()].clone(),
            was,
        });
    }
    if write_atomic(&share, MANIFEST, man.render().as_bytes(), 0o600).is_err() {
        return back_out("the manifest cannot be written");
    }

    for p in Part::ALL {
        if now[p.index()] == Current::Hash(new[p.index()].clone()) {
            if !fix_mode(dirs.of(p), p.file(), p.mode()) {
                return stopped(
                    CMD,
                    &format!("{} could not get mode {:o}", p.shown(), p.mode()),
                );
            }
            continue;
        }
        if write_atomic(dirs.of(p), p.file(), content(p), p.mode()).is_err() {
            return stopped(CMD, &format!("{} could not be written", p.shown()));
        }
        say(&format!("wrote {}", p.shown()));
        wrote += 1;
    }
    if wrote == 0 {
        say("files: up to date");
    }

    let timer_line;
    let mut timer_as_asked = true;
    let mut extra = Vec::new();
    match systemctl() {
        Ok(sc) => {
            let (ok, msg) = sc.run(&["daemon-reload"]);
            if !ok {
                extra.push(format!("systemctl --user daemon-reload failed: {msg}"));
            }
            let args: &[&str] = if timer {
                &["enable", "--now", TIMER_UNIT]
            } else {
                &["disable", "--now", TIMER_UNIT]
            };
            let (ok, msg) = sc.run(args);
            if ok {
                timer_line = if timer {
                    "timer: enabled and started (every :00 and :30)".into()
                } else {
                    "timer: off (--no-timer); tools that call get --refresh start fetches".into()
                };
            } else {
                timer_as_asked = false;
                timer_line = format!("timer: systemctl --user {} failed: {msg}", args.join(" "));
            }
        }
        Err(why) => {
            let linked = if timer {
                link_timer(&home, &dirs.units)
            } else {
                unlink_timer(&home, &dirs.units)
            };
            timer_line = match (timer, linked) {
                (true, true) if why == NO_SYSTEMCTL => format!(
                    "timer: enabled, but nothing runs it ({why}); tools that call get --refresh start fetches"
                ),
                (true, true) => format!(
                    "timer: enabled; it starts with your systemd user manager ({why}: at your next login)"
                ),
                (true, false) => {
                    timer_as_asked = false;
                    format!("timer: NOT enabled ({why})")
                }
                (false, _) => {
                    "timer: off (--no-timer); tools that call get --refresh start fetches".into()
                }
            };
        }
    }

    for r in man.files.iter_mut().flatten() {
        r.was = None;
    }
    man.pending = None;
    if write_atomic(&share, MANIFEST, man.render().as_bytes(), 0o600).is_err() {
        return stopped(CMD, "the manifest could not be finished");
    }
    say(&format!(
        "claude-usage {} installed in ~/{BIN_DIR}",
        env!("CARGO_PKG_VERSION")
    ));
    say(&timer_line);
    for e in extra {
        say(&format!("warning: {e}"));
    }
    for n in notes(&home) {
        say(&format!("note: {n}"));
    }
    say(STATUSLINE_NOTE);
    if !timer_as_asked {
        warn(&format!(
            "install: the files are installed, but the timer is not {} as asked: run claude-usage install again once systemd works",
            if timer { "enabled" } else { "off" }
        ));
        return 3;
    }
    0
}

fn dropin_hint(p: Part) -> String {
    match p {
        Part::Bin => String::new(),
        _ => format!(
            " (keep your changes in a drop-in, ~/{UNIT_DIR}/{}.d/, and remove the file to reinstall)",
            p.file()
        ),
    }
}

// ------------------------------------------------------------------ install --status

pub fn status() -> u8 {
    let Some(home) = paths::home() else {
        say("not installed (HOME is not set)");
        return 1;
    };
    let share = match open_share(&home, false) {
        Ok(Some(fd)) => fd,
        Ok(None) => {
            say("not installed");
            return 1;
        }
        Err(m) => {
            say(&format!("problem: {m}"));
            return 4;
        }
    };
    let mut problems = Vec::new();
    let mut info = Vec::new();
    if sys::flock(share.as_fd(), libc::LOCK_SH | libc::LOCK_NB).is_err() {
        problems.push("an install or uninstall is running".to_owned());
    }
    let man = match read_manifest(&share) {
        Ok(Some(m)) => m,
        Ok(None) => {
            say("not installed");
            return 1;
        }
        Err(m) => {
            say(&format!("problem: {m}"));
            return 4;
        }
    };
    say(&format!("claude-usage {} installed", man.version));
    match man.pending {
        Some(Pending::Install) => {
            problems.push("an install stopped part-way: run claude-usage install again".into())
        }
        Some(Pending::Uninstall) => {
            problems.push("an uninstall stopped part-way: run claude-usage uninstall again".into())
        }
        None => {}
    }
    let bin = open_dir(&home, BIN_DIR, false);
    let units = open_dir(&home, UNIT_DIR, false);
    for p in Part::ALL {
        let dir = match if p == Part::Bin { &bin } else { &units } {
            Ok(Some(d)) => d,
            Ok(None) => {
                problems.push(format!("{} is missing", p.shown()));
                continue;
            }
            Err(m) => {
                problems.push(m.clone());
                continue;
            }
        };
        let Some(rec) = &man.files[p.index()] else {
            problems.push(format!("{} is not in the manifest", p.shown()));
            continue;
        };
        match current(dir, p.file()) {
            Current::Absent => problems.push(format!("{} is missing", p.shown())),
            Current::Bad(why) => problems.push(format!("{} {why}", p.shown())),
            Current::Hash(h) if h == rec.sha => {}
            Current::Hash(_) => problems.push(format!(
                "{} was changed since the install{}",
                p.shown(),
                dropin_hint(p)
            )),
        }
    }
    if let (Ok(Some(dir)), Some(own)) = (&bin, own_bytes())
        && current(dir, NAME) != Current::Hash(sha_hex(&own))
    {
        info.push(
            "this claude-usage is not the installed one: claude-usage install from it installs it"
                .to_owned(),
        );
    }
    if let Ok(Some(u)) = &units {
        let link = link_state(&home, u);
        match (man.timer, &link) {
            (true, Link::Ours) | (false, Link::Absent) => {}
            (true, _) => problems.push(
                "the timer is wanted but not enabled: run claude-usage install --timer".into(),
            ),
            (false, _) => problems.push(format!(
                "the timer is off (--no-timer) but ~/{UNIT_DIR}/{WANTS}/{TIMER_UNIT} is there"
            )),
        }
        for d in [SERVICE_UNIT, TIMER_UNIT] {
            let n = format!("{d}.d");
            if sys::stat_at(u.as_fd(), OsStr::new(&n)).is_ok() {
                info.push(format!("~/{UNIT_DIR}/{n}/ is yours: its settings apply"));
            }
        }
        if man.timer {
            match systemctl() {
                Ok(sc) => {
                    if !sc.run(&["is-active", "--quiet", TIMER_UNIT]).0 {
                        problems.push("the timer is enabled but not active: systemctl --user start claude-usage.timer".into());
                    }
                }
                Err(why) => info.push(format!("whether the timer runs cannot be asked: {why}")),
            }
        }
    }
    say(&format!(
        "timer: {}",
        if man.timer { "on" } else { "off (--no-timer)" }
    ));
    info.extend(notes(&home));
    for i in &info {
        say(&format!("note: {i}"));
    }
    for p in &problems {
        say(&format!("problem: {p}"));
    }
    if problems.is_empty() {
        say("healthy");
        0
    } else {
        4
    }
}

// ------------------------------------------------------------------ uninstall

/// `~/.local/state/claude-usage/` as uninstall --purge accepts it: a folder of the user's
/// holding only `<key>/` folders (with `usage.json`, `.lock` and temp files) and `<key>.refused`
/// notes, all the user's, no symlink anywhere. The keys, or why not.
fn state_shape(root: &OwnedFd) -> Result<Vec<(String, OwnedFd)>, String> {
    let euid = sys::euid();
    let mut keys = Vec::new();
    let names = sys::names(root.as_fd()).map_err(|_| "it cannot be listed".to_owned())?;
    for n in names {
        let s = n
            .to_str()
            .ok_or("it holds a name that is not UTF-8")?
            .to_owned();
        let st = sys::stat_at(root.as_fd(), &n).map_err(|_| format!("{s} cannot be checked"))?;
        if st.uid != euid {
            return Err(format!("{s} is not yours"));
        }
        let key_ok = |k: &str| {
            k.len() == 16
                && k.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if st.is_file() && s.strip_suffix(".refused").is_some_and(key_ok) {
            continue;
        }
        if !(st.is_dir() && key_ok(&s)) {
            return Err(format!("{s} is not claude-usage's"));
        }
        let fd = sys::open_at(
            root.as_fd(),
            &s,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(|_| format!("{s} cannot be opened"))?;
        for m in sys::names(fd.as_fd()).map_err(|_| format!("{s} cannot be listed"))? {
            let b = m.as_bytes();
            let temp = b.len() == ".usage.json.".len() + 6 && b.starts_with(b".usage.json.");
            let ok_name = b == b"usage.json" || b == b".lock" || temp;
            let st = sys::stat_at(fd.as_fd(), &m).map_err(|_| format!("{s} cannot be checked"))?;
            if !ok_name || !st.is_file() || st.uid != euid {
                return Err(format!("{s}/{} is not claude-usage's", m.to_string_lossy()));
            }
        }
        keys.push((s, fd));
    }
    Ok(keys)
}

/// `~/.local/state/claude-usage/` (no symlink, the user's), or None when missing.
fn open_state_root(home: &Path) -> Result<Option<OwnedFd>, String> {
    let base = match sys::open(&home.join(STATE_DIR), libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(fd) => fd,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(format!("~/{STATE_DIR} cannot be opened")),
    };
    match sys::open_at(
        base.as_fd(),
        NAME,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    ) {
        Ok(fd) if sys::fstat(fd.as_fd()).is_ok_and(|st| st.uid == sys::euid()) => Ok(Some(fd)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        _ => Err(format!(
            "~/{STATE_DIR}/{NAME} is not a folder of yours (a symbolic link?)"
        )),
    }
}

pub fn uninstall(purge: bool) -> u8 {
    const CMD: &str = "uninstall";
    let home = match who_may_run() {
        Ok(h) => h,
        Err(m) => return refuse(CMD, &m),
    };
    let share = match open_share(&home, false) {
        Ok(s) => s,
        Err(m) => return refuse(CMD, &m),
    };
    if let Some(s) = &share
        && sys::flock(s.as_fd(), libc::LOCK_EX | libc::LOCK_NB).is_err()
    {
        return refuse(CMD, "another install or uninstall is running");
    }
    let man = match share.as_ref().map(read_manifest) {
        Some(Ok(m)) => m,
        Some(Err(m)) => return refuse(CMD, &m),
        None => None,
    };
    let state_root = match open_state_root(&home) {
        Ok(r) => r,
        Err(m) if purge => return refuse(CMD, &m),
        Err(_) => None,
    };
    if purge
        && let Some(r) = &state_root
        && let Err(m) = state_shape(r)
    {
        return refuse(
            CMD,
            &format!("--purge: ~/{STATE_DIR}/{NAME}/: {m}; nothing removed"),
        );
    }
    let Some(mut man) = man else {
        if !purge {
            if let Some(s) = &share {
                drop_share(&home, s);
            }
            say("not installed: nothing to do");
            return 0;
        }
        return match finish_state(&home, state_root, true) {
            Ok(()) => 0,
            Err(code) => code,
        };
    };
    man.pending = Some(Pending::Uninstall);
    let share = share.expect("a manifest was read from it");
    if write_atomic(&share, MANIFEST, man.render().as_bytes(), 0o600).is_err() {
        return refuse(CMD, "the manifest cannot be written");
    }

    // 1. The timer off and the service stopped.
    let sc = systemctl();
    if let Ok(sc) = &sc {
        let (ok, msg) = sc.run(&["disable", "--now", TIMER_UNIT]);
        if !ok {
            say(&format!(
                "warning: systemctl --user disable --now {TIMER_UNIT}: {msg}"
            ));
        }
        let (ok, msg) = sc.run(&["stop", SERVICE_UNIT]);
        if !ok {
            say(&format!(
                "warning: systemctl --user stop {SERVICE_UNIT}: {msg}"
            ));
        }
    }
    // 2. The program: a running get then spawns nothing, a fetch creates no state folder.
    // Its own files it could not remove, or a folder it cannot open: the manifest stays
    // (pending uninstall) and the run stops, so a rerun still knows them.
    let mut stuck = Vec::new();
    let mut folder = |rel: &str| match open_dir(&home, rel, false) {
        Ok(d) => d,
        Err(m) => {
            stuck.push(m);
            None
        }
    };
    let bin = folder(BIN_DIR);
    let units = folder(UNIT_DIR);
    let mut kept = Vec::new();
    let mut take = |dir: &Option<OwnedFd>, p: Part| {
        let Some(d) = dir else { return };
        match current(d, p.file()) {
            Current::Absent => {}
            Current::Hash(h) if man.owns(p, &h) => {
                if remove(d, p.file()) {
                    say(&format!("removed {}", p.shown()));
                } else {
                    stuck.push(format!("{} could not be removed", p.shown()));
                }
            }
            Current::Hash(_) => kept.push(format!("kept {}: changed since the install", p.shown())),
            Current::Bad(why) => kept.push(format!("kept {}: it {why}", p.shown())),
        }
    };
    take(&bin, Part::Bin);
    // 3. Running fetches finish: each state folder's lock, 15 s at most in all.
    let locks = match state_root.as_ref().map(lock_all) {
        Some(Err(m)) => {
            return stopped(
                CMD,
                &format!(
                    "{m} (the installed claude-usage is removed: run uninstall from the release's copy)"
                ),
            );
        }
        Some(Ok(l)) => l,
        None => Vec::new(),
    };
    // 4. The units and the timer's link.
    take(&units, Part::Service);
    take(&units, Part::Timer);
    if let Some(u) = &units {
        if !unlink_timer(&home, u) {
            kept.push(format!(
                "kept ~/{UNIT_DIR}/{WANTS}/{TIMER_UNIT}: not the timer's link"
            ));
        }
        for d in [SERVICE_UNIT, TIMER_UNIT] {
            let n = format!("{d}.d");
            if sys::stat_at(u.as_fd(), OsStr::new(&n)).is_ok() {
                kept.push(format!("kept ~/{UNIT_DIR}/{n}/: yours"));
            }
        }
    }
    if let Ok(sc) = &sc {
        let _ = sc.run(&["daemon-reload"]);
        let _ = sc.run(&["reset-failed", SERVICE_UNIT]);
    }
    if !stuck.is_empty() {
        for k in &kept {
            say(k);
        }
        return stopped(CMD, &stuck.join("; "));
    }
    // 5. The state, with --purge.
    if purge && let Err(code) = finish_state(&home, state_root, false) {
        return code;
    }
    drop(locks);
    // 6. The manifest last.
    if !remove(&share, MANIFEST) {
        return stopped(CMD, "the manifest could not be removed");
    }
    drop_share(&home, &share);
    for k in &kept {
        say(k);
    }
    say(if purge {
        "claude-usage removed, its state too"
    } else {
        "claude-usage removed; its state stays in ~/.local/state/claude-usage (uninstall --purge removes it)"
    });
    0
}

/// Each state folder's `.lock`, held until dropped: no fetch writes while they are.
fn lock_all(root: &OwnedFd) -> Result<Vec<File>, String> {
    let end = Instant::now() + LOCK_WAIT;
    let mut held = Vec::new();
    for n in sys::names(root.as_fd()).map_err(|_| "the state folder cannot be listed".to_owned())? {
        let Some(s) = n.to_str() else { continue };
        let Ok(dir) = sys::open_at(
            root.as_fd(),
            s,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        ) else {
            continue;
        };
        let Ok(fd) = sys::open_at(dir.as_fd(), ".lock", libc::O_RDWR | libc::O_NOFOLLOW, 0) else {
            continue;
        };
        let f = File::from(fd);
        match lock::acquire(&f, end.saturating_duration_since(Instant::now())) {
            Taken::Yes => held.push(f),
            Taken::Busy => {
                return Err(format!(
                    "a fetch still holds ~/{STATE_DIR}/{NAME}/{s}/.lock after 15 s"
                ));
            }
            Taken::Failed => {}
        }
    }
    Ok(held)
}

/// `--purge`: checks the shape again (under the locks) and removes the state folder.
fn finish_state(home: &Path, root: Option<OwnedFd>, take_locks: bool) -> Result<(), u8> {
    const CMD: &str = "uninstall";
    let Some(root) = root else {
        say("no state to remove");
        return Ok(());
    };
    let _locks = if take_locks {
        match lock_all(&root) {
            Ok(l) => l,
            Err(m) => return Err(stopped(CMD, &m)),
        }
    } else {
        Vec::new()
    };
    let keys = match state_shape(&root) {
        Ok(k) => k,
        // Nothing removed yet when this run took the locks itself (no manifest); else the
        // program and units are gone and the manifest stays pending.
        Err(m) if take_locks => {
            return Err(refuse(CMD, &format!("--purge: ~/{STATE_DIR}/{NAME}/: {m}")));
        }
        Err(m) => {
            return Err(stopped(
                CMD,
                &format!(
                    "--purge: ~/{STATE_DIR}/{NAME}/ changed: {m}; it stays (without --purge, uninstall finishes and keeps it)"
                ),
            ));
        }
    };
    for (k, fd) in keys {
        for n in sys::names(fd.as_fd()).unwrap_or_default() {
            let _ = sys::unlink_at(fd.as_fd(), &n);
        }
        if sys::rmdir_at(root.as_fd(), OsStr::new(&k)).is_err() {
            return Err(stopped(
                CMD,
                &format!("~/{STATE_DIR}/{NAME}/{k} could not be removed"),
            ));
        }
    }
    for n in sys::names(root.as_fd()).unwrap_or_default() {
        let _ = sys::unlink_at(root.as_fd(), &n);
    }
    let base = sys::open(&home.join(STATE_DIR), libc::O_RDONLY | libc::O_DIRECTORY);
    match base.map(|b| sys::rmdir_at(b.as_fd(), OsStr::new(NAME))) {
        Ok(Ok(())) => {
            say(&format!("removed ~/{STATE_DIR}/{NAME}"));
            Ok(())
        }
        _ => Err(stopped(
            CMD,
            &format!("~/{STATE_DIR}/{NAME} could not be removed"),
        )),
    }
}

/// Removes `~/.local/share/claude-usage/` when only temp files of the manifest are left.
fn drop_share(home: &Path, share: &OwnedFd) {
    sweep(share, &[MANIFEST]);
    if sys::names(share.as_fd()).is_ok_and(|n| n.is_empty())
        && let Ok(parent) = sys::open(&home.join(SHARE_DIR), libc::O_RDONLY | libc::O_DIRECTORY)
        && sys::rmdir_at(parent.as_fd(), OsStr::new(NAME)).is_ok()
    {
        let _ = sys::fsync(parent.as_fd());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            version: "0.1.0".into(),
            timer: false,
            pending: Some(Pending::Install),
            files: [
                Some(Record {
                    sha: "a".repeat(64),
                    was: Some("b".repeat(64)),
                }),
                Some(Record {
                    sha: "c".repeat(64),
                    was: None,
                }),
                None,
            ],
        }
    }

    #[test]
    fn manifest_round_trip() {
        let m = sample();
        let text = m.render();
        assert_eq!(
            text,
            format!(
                "{HEADER}\nversion 0.1.0\ntimer off\npending install\nfile bin {} {}\nfile service {} -\n",
                "a".repeat(64),
                "b".repeat(64),
                "c".repeat(64)
            )
        );
        assert_eq!(Manifest::parse(&text), Some(m));
    }

    #[test]
    fn manifest_is_strict() {
        let good = sample().render();
        assert!(Manifest::parse(&good).is_some());
        for bad in [
            good.replace(HEADER, "claude-usage manifest 2"),
            good.replace("timer off", "timer maybe"),
            good.replace("version 0.1.0\n", ""),
            good.replace("timer off\n", ""),
            format!("{good}version 0.2.0\n"),
            format!("{good}file service {} -\n", "d".repeat(64)),
            format!("{good}file other {} -\n", "d".repeat(64)),
            format!("{good}path /etc/passwd\n"),
            good.replace(&"c".repeat(64), &"C".repeat(64)),
            good.replace(&"c".repeat(64), &"c".repeat(63)),
            good.replace("version 0.1.0", "version 0.1.0 x"),
            good.replace("version 0.1.0", "version ../x/y"),
            good.replace("pending install", "pending install\npending uninstall"),
            good.replace("pending install\n", ""),
        ] {
            assert_eq!(Manifest::parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn owns_the_record_and_what_was_there() {
        let m = sample();
        assert!(m.owns(Part::Bin, &"a".repeat(64)));
        assert!(m.owns(Part::Bin, &"b".repeat(64)));
        assert!(!m.owns(Part::Bin, &"c".repeat(64)));
        assert!(m.owns(Part::Service, &"c".repeat(64)));
        assert!(!m.owns(Part::Timer, &"a".repeat(64)));
    }

    #[test]
    fn the_units_are_the_repos() {
        assert!(SERVICE_TEXT.contains("ExecStart=%h/.local/bin/claude-usage fetch\n"));
        assert!(TIMER_TEXT.contains("OnCalendar=*:*:00,30\n"));
    }
}
