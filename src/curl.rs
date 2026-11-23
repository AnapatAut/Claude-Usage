//! The call: a curl child with a fixed argument list, a cleared environment, the token on its
//! stdin, a 10 s watchdog and a 64 KiB cap on what it prints.

use crate::secret::Secret;
use crate::sys;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The argument list, fixed. `-q` first: curl then reads no `.curlrc`.
pub const ARGS: [&str; 18] = [
    "-q",
    "-sS",
    "--max-time",
    "5",
    "--connect-timeout",
    "3",
    "--proto",
    "=https",
    "--max-redirs",
    "0",
    "-H",
    "@-",
    "-H",
    "anthropic-beta: oauth-2025-04-20",
    "-o",
    "-",
    "-w",
    // A backslash and an n: curl turns them into a newline itself.
    "\\n%{http_code} %header{retry-after}",
];
const URL: &str = "https://api.anthropic.com/api/oauth/usage";

const WATCHDOG: Duration = Duration::from_secs(10);
/// The body cap, plus room for the `-w` tail.
const MAX_OUT: usize = 64 * 1024 + 128;
/// Not on PATH: a project's PATH holds folders the user's other software writes.
const SYSTEM: [&str; 3] = ["/usr/bin/curl", "/bin/curl", "/usr/local/bin/curl"];

/// The curl to run: the config file's `curl =` (absolute, in folders of root or the user that
/// group and others cannot write), else the first of the system paths in root-owned folders.
/// None: `no_curl`.
pub fn find(configured: Option<&Path>) -> Option<PathBuf> {
    match configured {
        Some(p) => trusted(p, true),
        None => SYSTEM.iter().find_map(|p| trusted(Path::new(p), false)),
    }
}

/// `p`, resolved, when it and every folder above it belong to root (or, with `user_ok`, to the
/// user) and group and others cannot write them.
pub fn trusted(p: &Path, user_ok: bool) -> Option<PathBuf> {
    if !p.is_absolute() {
        return None;
    }
    let real = std::fs::canonicalize(p).ok()?;
    let euid = sys::euid();
    let owner_ok = |uid: u32| uid == 0 || (user_ok && uid == euid);
    let m = std::fs::metadata(&real).ok()?;
    if !m.is_file() || !owner_ok(m.uid()) || m.mode() & 0o022 != 0 {
        return None;
    }
    for dir in real.ancestors().skip(1) {
        let m = std::fs::metadata(dir).ok()?;
        if !m.is_dir() || !owner_ok(m.uid()) || m.mode() & 0o022 != 0 {
            return None;
        }
    }
    Some(real)
}

/// How the call ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// curl did not start, exited 1, 2 or 4, or printed a `-w` tail it did not expand.
    NoCurl,
    /// Any other non-zero exit, a signal, the watchdog.
    Network,
    /// Printed more than the cap.
    TooBig,
    /// A response: the status, the body, the `Retry-After` value ("" when absent).
    Response {
        status: u16,
        body: Vec<u8>,
        retry_after: String,
    },
}

pub fn call(curl: &Path, header: &Secret) -> Ended {
    let mut cmd = Command::new(curl);
    cmd.args(ARGS)
        .arg(URL)
        .env_clear()
        .env("LANG", "C")
        .env("PATH", "/usr/bin:/bin")
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // SAFETY: the hook makes only system calls (no allocation, no locks).
    unsafe {
        cmd.pre_exec(|| {
            sys::cloexec_from(3);
            Ok(())
        });
    }
    let Ok(mut child) = cmd.spawn() else {
        return Ended::NoCurl;
    };
    let pid = libc::pid_t::try_from(child.id()).unwrap_or(0);
    let out = pump(&mut child, header.as_bytes());
    let killed = out.is_err();
    if killed {
        sys::kill_group(pid);
    }
    let status = wait(&mut child, killed, pid);
    match out {
        Err(Over::Time) => Ended::Network,
        Err(Over::Size) => Ended::TooBig,
        Ok(stdout) => match status.and_then(|s| s.code()) {
            Some(0) => tail(stdout),
            Some(1 | 2 | 4) => Ended::NoCurl,
            _ => Ended::Network,
        },
    }
}

enum Over {
    Time,
    Size,
}

/// Feeds stdin and drains stdout and stderr until both close, within the watchdog. stderr is
/// read and dropped, never stored or forwarded.
fn pump(child: &mut Child, input: &[u8]) -> Result<Vec<u8>, Over> {
    let deadline = Instant::now() + WATCHDOG;
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if let Some(s) = &stdin
        && sys::set_nonblocking(s.as_fd()).is_err()
    {
        stdin = None;
    }
    let mut sent = 0;
    let mut out = Vec::new();
    let mut scratch = [0u8; 4096];
    while stdout.is_some() || stderr.is_some() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Over::Time);
        }
        let mut fds = Vec::with_capacity(3);
        let mut push = |fd: i32, ev: libc::c_short| {
            fds.push(libc::pollfd {
                fd,
                events: ev,
                revents: 0,
            })
        };
        push(stdin.as_ref().map_or(-1, |s| s.as_raw_fd()), libc::POLLOUT);
        push(stdout.as_ref().map_or(-1, |s| s.as_raw_fd()), libc::POLLIN);
        push(stderr.as_ref().map_or(-1, |s| s.as_raw_fd()), libc::POLLIN);
        let ms = libc::c_int::try_from(left.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
        // SAFETY: fds is a valid array of pollfd; negative fds are ignored by poll.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 3, ms) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(Over::Time);
        }
        if fds[0].revents != 0 {
            let done = match stdin.as_mut().map(|s| s.write(&input[sent..])) {
                Some(Ok(n)) => {
                    sent += n;
                    sent == input.len()
                }
                Some(Err(e)) => {
                    e.kind() != ErrorKind::WouldBlock && e.kind() != ErrorKind::Interrupted
                }
                None => true,
            };
            if done {
                stdin = None; // closed: curl reads -H @- to EOF
            }
        }
        if fds[1].revents != 0
            && let Some(s) = stdout.as_mut()
        {
            match s.read(&mut scratch) {
                Ok(0) => stdout = None,
                Ok(n) => {
                    if out.len() + n > MAX_OUT {
                        return Err(Over::Size);
                    }
                    out.extend_from_slice(&scratch[..n]);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => stdout = None,
            }
        }
        if fds[2].revents != 0
            && let Some(s) = stderr.as_mut()
        {
            match s.read(&mut scratch) {
                Ok(0) => stderr = None,
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => stderr = None,
            }
        }
    }
    Ok(out)
}

/// Reaps curl. After a kill: at once, no status. Otherwise it closed its output and gets one
/// more second to exit; past that it is killed and has no status (`network`).
fn wait(child: &mut Child, killed: bool, pid: libc::pid_t) -> Option<std::process::ExitStatus> {
    if killed {
        let _ = child.wait();
        return None;
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                sys::kill_group(pid);
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Splits curl's output at the last newline: the body, then `<3 digits> <retry-after>`. A curl
/// older than 7.84 leaves `%header{…}` unexpanded: the call was made, so its status counts (a 429
/// still backs off), with no Retry-After. Any other tail is `no_curl`.
fn tail(mut out: Vec<u8>) -> Ended {
    let Some(nl) = out.iter().rposition(|&b| b == b'\n') else {
        return Ended::NoCurl;
    };
    let t = out.split_off(nl + 1);
    out.pop();
    let Ok(t) = std::str::from_utf8(&t) else {
        return Ended::NoCurl;
    };
    let (code, value) = match t.split_once(' ') {
        Some(cv) => cv,
        None => return Ended::NoCurl,
    };
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Ended::NoCurl;
    }
    let value = if value.contains("%header{") {
        ""
    } else {
        value
    };
    let status = code.parse().unwrap_or(0);
    if status == 0 {
        return Ended::Network;
    }
    Ended::Response {
        status,
        body: out,
        retry_after: value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, body: &[u8], ra: &str) -> Ended {
        Ended::Response {
            status,
            body: body.to_vec(),
            retry_after: ra.into(),
        }
    }

    #[test]
    fn tails() {
        assert_eq!(tail(b"{}\n200 ".to_vec()), resp(200, b"{}", ""));
        assert_eq!(tail(b"a\nb\n429 120".to_vec()), resp(429, b"a\nb", "120"));
        assert_eq!(
            tail(b"x\n429 Tue, 17 Nov 2026 05:30:00 GMT".to_vec()),
            resp(429, b"x", "Tue, 17 Nov 2026 05:30:00 GMT")
        );
        assert_eq!(tail(b"\n200 ".to_vec()), resp(200, b"", ""));
        assert_eq!(tail(b"{}\n000 ".to_vec()), Ended::Network);
        // curl before 7.84: the status counts, no Retry-After.
        assert_eq!(
            tail(b"{}\n200 %header{retry-after}".to_vec()),
            resp(200, b"{}", "")
        );
        assert_eq!(
            tail(b"{}\n429 %header{retry-after}".to_vec()),
            resp(429, b"{}", "")
        );
        for bad in [&b"{}"[..], b"{}\n200", b"{}\n20 x", b"{}\nabc ", b""] {
            assert_eq!(
                tail(bad.to_vec()),
                Ended::NoCurl,
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn argv_is_the_designed_list() {
        let mut all: Vec<&str> = ARGS.to_vec();
        all.push(URL);
        assert_eq!(
            all.join(" "),
            "-q -sS --max-time 5 --connect-timeout 3 --proto =https --max-redirs 0 -H @- \
             -H anthropic-beta: oauth-2025-04-20 -o - -w \\n%{http_code} %header{retry-after} \
             https://api.anthropic.com/api/oauth/usage"
        );
    }

    #[test]
    fn relative_or_missing_curl_is_refused() {
        assert_eq!(find(Some(Path::new("bin/curl"))), None);
        assert_eq!(find(Some(Path::new("/nonexistent/curl"))), None);
        assert_eq!(trusted(Path::new("/tmp"), true), None); // not a file
    }
}
