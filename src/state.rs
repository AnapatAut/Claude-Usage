//! The state: `$HOME/.local/state/claude-usage/<key>/usage.json`, its folders, and the refusal
//! note. Every open and rename goes through folder fds (`O_NOFOLLOW` covers only the
//! last component of a path; a check-then-open races).

use crate::parse::{Window, Windows, pct_ok};
use crate::paths::hex;
use crate::sys;
use serde_json::{Map, Value};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

pub const VERSION: i64 = 1;
const MAX_BYTES: usize = 64 * 1024;
const TEMP_PREFIX: &str = ".usage.json.";

/// What decides the next call. An unknown state version keeps exactly these, so that an
/// upgrade or a downgrade never loses a backoff.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sched {
    pub last_attempt: Option<i64>,
    /// The earliest next call under a backoff or hold; None = normal.
    pub next_attempt: Option<i64>,
    pub backoff_step: u32,
    pub fail_streak: u32,
    pub last_error: Option<String>,
    pub retry_after_seen: Option<bool>,
    /// `--now` calls that were not otherwise due.
    pub now_times: Vec<i64>,
}

/// `usage.json`, version 1. The format is fixed by the design (section 5, State file).
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub config_dir: String,
    pub salt: String,
    pub attempt_fingerprint: Option<String>,
    pub numbers_fingerprint: Option<String>,
    pub attempt_stamp: Option<String>,
    pub numbers_stamp: Option<String>,
    pub fetched_at: Option<i64>,
    pub windows: Windows,
    pub pending: bool,
    pub refusal: Option<String>,
    pub sched: Sched,
}

impl State {
    pub fn new(config_dir: String, salt: String, sched: Sched) -> State {
        State {
            config_dir,
            salt,
            attempt_fingerprint: None,
            numbers_fingerprint: None,
            attempt_stamp: None,
            numbers_stamp: None,
            fetched_at: None,
            windows: Windows::default(),
            pending: false,
            refusal: None,
            sched,
        }
    }

    pub fn to_json(&self) -> Vec<u8> {
        let opt_s = |v: &Option<String>| v.clone().map_or(Value::Null, Value::String);
        let opt_i = |v: Option<i64>| v.map_or(Value::Null, Value::from);
        let mut m = Map::new();
        m.insert("state_version".into(), VERSION.into());
        m.insert("config_dir".into(), self.config_dir.clone().into());
        m.insert("salt".into(), self.salt.clone().into());
        m.insert(
            "attempt_fingerprint".into(),
            opt_s(&self.attempt_fingerprint),
        );
        m.insert(
            "numbers_fingerprint".into(),
            opt_s(&self.numbers_fingerprint),
        );
        m.insert("attempt_stamp".into(), opt_s(&self.attempt_stamp));
        m.insert("numbers_stamp".into(), opt_s(&self.numbers_stamp));
        m.insert("fetched_at".into(), opt_i(self.fetched_at));
        m.insert("windows".into(), windows_json(&self.windows));
        m.insert("last_attempt".into(), opt_i(self.sched.last_attempt));
        m.insert("pending".into(), self.pending.into());
        m.insert("last_error".into(), opt_s(&self.sched.last_error));
        m.insert("next_attempt".into(), opt_i(self.sched.next_attempt));
        m.insert("backoff_step".into(), self.sched.backoff_step.into());
        m.insert("fail_streak".into(), self.sched.fail_streak.into());
        m.insert("now_times".into(), self.sched.now_times.clone().into());
        m.insert(
            "retry_after_seen".into(),
            self.sched.retry_after_seen.map_or(Value::Null, Value::Bool),
        );
        m.insert("refusal".into(), opt_s(&self.refusal));
        let mut out = Value::Object(m).to_string().into_bytes();
        out.push(b'\n');
        out
    }
}

pub fn window_json(w: &Option<Window>) -> Value {
    match w {
        None => Value::Null,
        Some(w) => {
            let mut m = Map::new();
            m.insert("pct".into(), Value::Number(w.pct.clone()));
            m.insert(
                "resets_at".into(),
                w.resets_at.map_or(Value::Null, Value::from),
            );
            Value::Object(m)
        }
    }
}

fn windows_json(w: &Windows) -> Value {
    let mut m = Map::new();
    m.insert("five_hour".into(), window_json(&w.five_hour));
    m.insert("seven_day".into(), window_json(&w.seven_day));
    Value::Object(m)
}

/// What a `usage.json` held.
#[derive(Debug, PartialEq)]
pub enum Loaded {
    /// A complete version-1 state.
    Full(Box<State>),
    /// Another version, or a version-1 file with a bad field: only the schedule is kept.
    Kept(Sched),
    /// Empty, not JSON, not an object.
    Empty,
}

pub fn parse(bytes: &[u8]) -> Loaded {
    let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(bytes) else {
        return Loaded::Empty;
    };
    if m.get("state_version").and_then(int) == Some(VERSION)
        && let Some(s) = full(&m)
    {
        return Loaded::Full(Box::new(s));
    }
    Loaded::Kept(kept(&m))
}

/// A whole number: JSON integers, and floats with no fraction (as jq may write them).
fn int(v: &Value) -> Option<i64> {
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    let f = v.as_f64()?;
    (f.fract() == 0.0 && f.abs() < 9.0e15).then_some(f as i64)
}

pub fn word_ok(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

fn hex_ok(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn stamp_ok(s: &str) -> bool {
    s.len() <= 128 && s.split(':').count() == 5 && s.split(':').all(|p| p.parse::<i128>().is_ok())
}

/// A field that is `null` or a valid value: outer None when present with a wrong type or shape.
fn nullable<T>(
    m: &Map<String, Value>,
    k: &str,
    f: impl Fn(&Value) -> Option<T>,
) -> Option<Option<T>> {
    match m.get(k)? {
        Value::Null => Some(None),
        v => f(v).map(Some),
    }
}

fn string_if(ok: impl Fn(&str) -> bool) -> impl Fn(&Value) -> Option<String> {
    move |v| v.as_str().filter(|s| ok(s)).map(str::to_owned)
}

fn small(v: &Value) -> Option<u32> {
    int(v)
        .and_then(|i| u32::try_from(i).ok())
        .filter(|&i| i <= 64)
}

fn times(v: &Value) -> Option<Vec<i64>> {
    let a = v.as_array()?;
    if a.len() > 64 {
        return None;
    }
    a.iter().map(int).collect()
}

fn window_from(v: &Value) -> Option<Option<Window>> {
    match v {
        Value::Null => Some(None),
        Value::Object(o) => {
            let pct = match o.get("pct")? {
                Value::Number(n) if pct_ok(n) => n.clone(),
                _ => return None,
            };
            let resets_at = nullable(o, "resets_at", int)?;
            Some(Some(Window { pct, resets_at }))
        }
        _ => None,
    }
}

fn full(m: &Map<String, Value>) -> Option<State> {
    let w = m.get("windows")?.as_object()?;
    let sched = Sched {
        last_attempt: nullable(m, "last_attempt", int)?,
        next_attempt: nullable(m, "next_attempt", int)?,
        backoff_step: small(m.get("backoff_step")?)?,
        fail_streak: small(m.get("fail_streak")?)?,
        last_error: nullable(m, "last_error", string_if(word_ok))?,
        retry_after_seen: nullable(m, "retry_after_seen", Value::as_bool)?,
        now_times: times(m.get("now_times")?)?,
    };
    Some(State {
        config_dir: m.get("config_dir")?.as_str()?.to_owned(),
        salt: string_if(|s| hex_ok(s, 32))(m.get("salt")?)?,
        attempt_fingerprint: nullable(m, "attempt_fingerprint", string_if(|s| hex_ok(s, 64)))?,
        numbers_fingerprint: nullable(m, "numbers_fingerprint", string_if(|s| hex_ok(s, 64)))?,
        attempt_stamp: nullable(m, "attempt_stamp", string_if(stamp_ok))?,
        numbers_stamp: nullable(m, "numbers_stamp", string_if(stamp_ok))?,
        fetched_at: nullable(m, "fetched_at", int)?,
        windows: Windows {
            five_hour: window_from(w.get("five_hour")?)?,
            seven_day: window_from(w.get("seven_day")?)?,
        },
        pending: m.get("pending")?.as_bool()?,
        refusal: nullable(m, "refusal", string_if(word_ok))?,
        sched,
    })
}

/// The schedule of any JSON object, field by field, where each parses.
fn kept(m: &Map<String, Value>) -> Sched {
    let get = |k: &str| m.get(k);
    Sched {
        last_attempt: get("last_attempt").and_then(int),
        next_attempt: get("next_attempt").and_then(int),
        backoff_step: get("backoff_step").and_then(small).unwrap_or(0),
        fail_streak: get("fail_streak").and_then(small).unwrap_or(0),
        last_error: get("last_error").and_then(string_if(word_ok)),
        retry_after_seen: get("retry_after_seen").and_then(Value::as_bool),
        now_times: get("now_times").and_then(times).unwrap_or_default(),
    }
}

/// A new random salt for the credentials fingerprint (not a secret).
pub fn new_salt() -> io::Result<String> {
    let mut b = [0u8; 16];
    sys::random(&mut b)?;
    Ok(hex(&b))
}

// ------------------------------------------------------------------ folders and files

#[derive(Debug, PartialEq, Eq)]
pub enum Problem {
    Missing,
    /// A symlink, not a folder or file, not the user's, or not openable.
    Unusable,
}

/// `claude-usage/` (for the refusal note) and `claude-usage/<key>/`.
pub struct Folder {
    pub root: Option<OwnedFd>,
    pub dir: Result<OwnedFd, Problem>,
}

/// How a folder is opened: `get` and `status` only look; `fetch` repairs a wrong mode on a
/// folder of the user's, and creates what is missing unless `create` is false.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Look,
    Write { create: bool },
}

pub fn open_folder(home: &Path, key: &str, access: Access) -> Folder {
    let base = home.join(".local/state");
    let fail = |p| Folder {
        root: None,
        dir: Err(p),
    };
    if access == (Access::Write { create: true })
        && std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .is_err()
    {
        return fail(Problem::Unusable);
    }
    let base = match sys::open(&base, libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(fd) => fd,
        Err(e) if sys::errno_is(&e, libc::ENOENT) => return fail(Problem::Missing),
        Err(_) => return fail(Problem::Unusable),
    };
    let root = match sub(&base, "claude-usage", access) {
        Ok(fd) => fd,
        Err(p) => return fail(p),
    };
    let dir = sub(&root, key, access);
    Folder {
        root: Some(root),
        dir,
    }
}

/// A folder of the user's, mode 700, never through a symlink.
fn sub(parent: &OwnedFd, name: &str, access: Access) -> Result<OwnedFd, Problem> {
    if access == (Access::Write { create: true }) {
        match sys::mkdir_at(parent.as_fd(), name, 0o700) {
            Ok(()) => {}
            Err(e) if sys::errno_is(&e, libc::EEXIST) => {}
            Err(_) => return Err(Problem::Unusable),
        }
    }
    let fd = match sys::open_at(
        parent.as_fd(),
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    ) {
        Ok(fd) => fd,
        Err(e) if sys::errno_is(&e, libc::ENOENT) => return Err(Problem::Missing),
        Err(_) => return Err(Problem::Unusable),
    };
    let st = sys::fstat(fd.as_fd()).map_err(|_| Problem::Unusable)?;
    if !st.is_dir() || st.uid != sys::euid() {
        return Err(Problem::Unusable);
    }
    if st.perm() != 0o700 && access != Access::Look {
        sys::fchmod(fd.as_fd(), 0o700).map_err(|_| Problem::Unusable)?;
    }
    Ok(fd)
}

/// Reads a small regular file of the user's in a folder, never through a symlink. Ok(None):
/// missing, or larger than `max` (as if missing). Err: not a regular file of the user's.
fn read_small(dir: &OwnedFd, name: &str, max: usize) -> Result<Option<Vec<u8>>, Problem> {
    let fd = match sys::open_at(
        dir.as_fd(),
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    ) {
        Ok(fd) => fd,
        Err(e) if sys::errno_is(&e, libc::ENOENT) => return Ok(None),
        Err(_) => return Err(Problem::Unusable),
    };
    let st = sys::fstat(fd.as_fd()).map_err(|_| Problem::Unusable)?;
    if !st.is_file() || st.uid != sys::euid() {
        return Err(Problem::Unusable);
    }
    let mut buf = Vec::new();
    let limit = u64::try_from(max).unwrap_or(u64::MAX) + 1;
    File::from(fd)
        .take(limit)
        .read_to_end(&mut buf)
        .map_err(|_| Problem::Unusable)?;
    Ok((buf.len() <= max).then_some(buf))
}

/// `usage.json`: Ok(None) when missing or over 64 KiB (no state); Err when it is a symlink, not
/// a regular file, or not the user's (`state_unusable`).
pub fn read_file(dir: &OwnedFd) -> Result<Option<Vec<u8>>, Problem> {
    read_small(dir, "usage.json", MAX_BYTES)
}

fn temp_name() -> io::Result<String> {
    const A: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut r = [0u8; 6];
    sys::random(&mut r)?;
    let mut s = String::from(TEMP_PREFIX);
    s.extend(r.iter().map(|b| char::from(A[usize::from(*b) % A.len()])));
    Ok(s)
}

/// Writes `usage.json` atomically: a new temp file (O_EXCL, 0600) in the same folder, then
/// rename. On failure the temp file is removed.
/// No fsync: the timer writes twice every 30 s for hours, and the state is a cache. Readers see
/// the old file or the new one, never a mix, and a killed fetch keeps its write-ahead record (the
/// page cache survives a process). Only a crash of the whole machine (power cut, kernel panic,
/// hard reset) can lose the last writes or leave an empty file; an empty or unreadable file is a
/// new state, so the worst case is a few extra calls (a lost 429 backoff starts again at its
/// first step).
pub fn write_file(dir: &OwnedFd, bytes: &[u8]) -> io::Result<()> {
    let mut made = None;
    for _ in 0..16 {
        let name = temp_name()?;
        match sys::open_at(
            dir.as_fd(),
            &name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        ) {
            Ok(fd) => {
                made = Some((name, fd));
                break;
            }
            Err(e) if sys::errno_is(&e, libc::EEXIST) => continue,
            Err(e) => return Err(e),
        }
    }
    let Some((name, fd)) = made else {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    };
    let mut f = File::from(fd);
    let r = f
        .write_all(bytes)
        .and_then(|()| sys::rename_at(dir.as_fd(), &name, "usage.json"));
    if r.is_err() {
        let _ = sys::unlink_at(dir.as_fd(), OsStr::new(&name));
    }
    r
}

/// Removes temp files of `write_file` older than 60 s (left by a killed fetch).
pub fn sweep_temps(dir: &OwnedFd, now: i64) {
    let Ok(names) = sys::names(dir.as_fd()) else {
        return;
    };
    for n in names {
        let b = n.as_bytes();
        if b.len() != TEMP_PREFIX.len() + 6 || !b.starts_with(TEMP_PREFIX.as_bytes()) {
            continue;
        }
        if let Ok(st) = sys::stat_at(dir.as_fd(), &n)
            && st.is_file()
            && st.uid == sys::euid()
            && now.saturating_sub(st.mtime) > 60
        {
            let _ = sys::unlink_at(dir.as_fd(), &n);
        }
    }
}

// ------------------------------------------------------------------ the refusal note

fn note_name(key: &str) -> String {
    format!("{key}.refused")
}

/// The refusal cause recorded beside the folder, when the state itself could not hold it.
pub fn read_note(root: &OwnedFd, key: &str) -> Option<String> {
    let b = read_small(root, &note_name(key), 64).ok()??;
    let s = String::from_utf8(b).ok()?;
    let s = s.trim_end();
    word_ok(s).then(|| s.to_owned())
}

/// Records a refusal cause beside the folder. On a full disk the file may stay empty: its time
/// still tells readers that a fetch just refused, and an empty note records no cause.
pub fn write_note(root: &OwnedFd, key: &str, cause: &str) {
    if let Ok(fd) = sys::open_at(
        root.as_fd(),
        &note_name(key),
        libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW,
        0o600,
    ) {
        let _ = File::from(fd).write_all(format!("{cause}\n").as_bytes());
    }
}

/// When the refusal note was last written (each refusal rewrites it).
pub fn note_time(root: &OwnedFd, key: &str) -> Option<i64> {
    sys::stat_at(root.as_fd(), OsStr::new(&note_name(key)))
        .ok()
        .filter(|st| st.is_file())
        .map(|st| st.mtime)
}

pub fn clear_note(root: &OwnedFd, key: &str) {
    let _ = sys::unlink_at(root.as_fd(), OsStr::new(&note_name(key)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> State {
        let mut s = State::new(
            "/nonexistent-u/.claude".into(),
            "0123456789abcdef0123456789abcdef".into(),
            Sched {
                last_attempt: Some(1_794_893_100),
                next_attempt: Some(1_794_893_160),
                backoff_step: 1,
                fail_streak: 0,
                last_error: Some("rate_limited".into()),
                retry_after_seen: Some(false),
                now_times: vec![1_794_893_000],
            },
        );
        s.attempt_fingerprint = Some("a".repeat(64));
        s.numbers_fingerprint = Some("b".repeat(64));
        s.attempt_stamp = Some("1:2:3:4:5".into());
        s.numbers_stamp = Some("1:2:3:4:6".into());
        s.fetched_at = Some(1_794_893_000);
        s.windows.five_hour = Some(Window {
            pct: serde_json::from_str("30.4").expect("n"),
            resets_at: Some(1_794_900_300),
        });
        s
    }

    #[test]
    fn round_trip() {
        let s = sample();
        assert_eq!(parse(&s.to_json()), Loaded::Full(Box::new(s)));
    }

    #[test]
    fn exactly_the_design_keys() {
        let v: Value = serde_json::from_slice(&sample().to_json()).expect("json");
        let keys: Vec<&str> = v
            .as_object()
            .expect("obj")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "attempt_fingerprint",
                "attempt_stamp",
                "backoff_step",
                "config_dir",
                "fail_streak",
                "fetched_at",
                "last_attempt",
                "last_error",
                "next_attempt",
                "now_times",
                "numbers_fingerprint",
                "numbers_stamp",
                "pending",
                "refusal",
                "retry_after_seen",
                "salt",
                "state_version",
                "windows"
            ]
        );
    }

    #[test]
    fn unknown_version_keeps_the_schedule() {
        let b = br#"{"state_version":99,"last_attempt":100,"next_attempt":700,"backoff_step":3,"fail_streak":2,"last_error":"rate_limited","retry_after_seen":true,"now_times":[50],"fetched_at":1}"#;
        assert_eq!(
            parse(b),
            Loaded::Kept(Sched {
                last_attempt: Some(100),
                next_attempt: Some(700),
                backoff_step: 3,
                fail_streak: 2,
                last_error: Some("rate_limited".into()),
                retry_after_seen: Some(true),
                now_times: vec![50],
            })
        );
    }

    #[test]
    fn a_bad_field_keeps_only_the_schedule() {
        let mut v: Value = serde_json::from_slice(&sample().to_json()).expect("json");
        v["salt"] = "XYZ".into();
        match parse(v.to_string().as_bytes()) {
            Loaded::Kept(k) => assert_eq!(k, sample().sched),
            _ => panic!("expected Kept"),
        }
        v["salt"] = "0123456789abcdef0123456789abcdef".into();
        v["windows"]["five_hour"]["pct"] = 2000.into();
        assert!(matches!(parse(v.to_string().as_bytes()), Loaded::Kept(_)));
        v["windows"]["five_hour"]["pct"] = 5.into();
        v["last_error"] = "Not A Word".into();
        assert!(matches!(parse(v.to_string().as_bytes()), Loaded::Kept(_)));
    }

    #[test]
    fn each_bad_field_keeps_only_the_schedule() {
        let good: Value = serde_json::from_slice(&sample().to_json()).expect("json");
        let bad: [(&str, Value); 18] = [
            ("config_dir", 5.into()),
            ("salt", "0123".into()),
            ("attempt_fingerprint", "x".repeat(64).into()),
            ("numbers_fingerprint", 1.into()),
            ("attempt_stamp", "1:2:3:4".into()),
            ("numbers_stamp", "a:b:c:d:e".into()),
            ("fetched_at", "1".into()),
            ("windows", Value::Null),
            ("last_attempt", 1.5.into()),
            ("pending", Value::Null),
            ("last_error", 7.into()),
            ("next_attempt", true.into()),
            ("backoff_step", 65.into()),
            ("fail_streak", (-1).into()),
            ("now_times", vec![1_i64; 65].into()),
            ("retry_after_seen", "no".into()),
            ("refusal", "Not-A-Word".into()),
            ("state_version", 2.into()),
        ];
        for (k, v) in bad {
            let mut m = good.clone();
            m[k] = v;
            assert!(
                matches!(parse(m.to_string().as_bytes()), Loaded::Kept(_)),
                "{k}"
            );
            let mut m = good.clone();
            m.as_object_mut().expect("obj").remove(k);
            assert!(
                matches!(parse(m.to_string().as_bytes()), Loaded::Kept(_)),
                "{k} missing"
            );
        }
        // The fields that do parse are kept.
        let mut m = good.clone();
        m["backoff_step"] = 99.into();
        match parse(m.to_string().as_bytes()) {
            Loaded::Kept(k) => {
                assert_eq!(k.backoff_step, 0);
                assert_eq!(k.next_attempt, sample().sched.next_attempt);
                assert_eq!(k.last_error, sample().sched.last_error);
            }
            _ => panic!("expected Kept"),
        }
    }

    #[test]
    fn garbage_is_empty() {
        for b in [&b""[..], b"garbage{", b"[1]", b"null", b"3"] {
            assert_eq!(parse(b), Loaded::Empty);
        }
    }

    #[test]
    fn jq_floats_are_whole_numbers() {
        let mut v: Value = serde_json::from_slice(&sample().to_json()).expect("json");
        v["last_attempt"] = serde_json::json!(1_794_893_100.0);
        match parse(v.to_string().as_bytes()) {
            Loaded::Full(s) => assert_eq!(s.sched.last_attempt, Some(1_794_893_100)),
            _ => panic!("expected Full"),
        }
    }
}
