//! `fetch [--now]`: one call to the usage endpoint when due (design section 5).

use crate::config;
use crate::creds::{self, Creds};
use crate::curl::{self, Ended};
use crate::lock;
use crate::parse::{self, Windows};
use crate::paths::{self, ConfigError, Exe};
use crate::schedule::{self, Outcome};
use crate::state::{self, Access, Loaded, Problem, Sched, State};
use crate::sys;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::time::Duration;

/// Every fetch ends by itself after this many seconds (SIGALRM; the kernel drops the lock): a
/// hang outside curl cannot hold the lock longer. Armed at start and again once the lock is
/// held, so that `--now`'s wait for the lock does not eat into it.
const DEADLINE: libc::c_uint = 25;

/// Why a fetch refused to run (exit 1).
#[derive(Clone, Copy)]
enum Cause {
    Root,
    Setuid,
    Home,
    Config,
    State,
    Write,
    Lock,
    Credentials,
}

impl Cause {
    /// Recorded, to say each cause once.
    fn word(self) -> &'static str {
        match self {
            Cause::Root => "root",
            Cause::Setuid => "setuid",
            Cause::Home => "home",
            Cause::Config => "bad_config",
            Cause::State => "state_unusable",
            Cause::Write => "write_failed",
            Cause::Lock => "lock_failed",
            Cause::Credentials => "credentials_not_yours",
        }
    }

    fn text(self) -> &'static str {
        match self {
            Cause::Root => "running as root",
            Cause::Setuid => "the real and effective user differ",
            Cause::Home => "HOME is not an absolute path to a folder you own",
            Cause::Config => "CLAUDE_CONFIG_DIR is a relative path",
            Cause::State => {
                "the state folder is unusable (a symlink, not a folder or file, or not yours)"
            }
            Cause::Write => "the state could not be written (read-only or full?)",
            Cause::Lock => "the lock file cannot be locked (a filesystem without flock?)",
            Cause::Credentials => "the credentials file is not yours",
        }
    }
}

/// Where a refusal is recorded when the state cannot hold it: `claude-usage/<key>.refused`.
#[derive(Default)]
struct Note {
    root: Option<OwnedFd>,
    key: Option<String>,
}

impl Note {
    /// Says a refusal once per change of cause (one `<4>` line): recorded in the state when it
    /// can be written, else beside the folder; printed every run when neither can be written.
    /// A fetch whose stderr is /dev/null (detached by `get --refresh`) is heard by nobody, so it
    /// never records a cause as said.
    fn refuse(&self, cause: Cause, in_state: Option<(&OwnedFd, &mut State)>) -> u8 {
        let w = cause.word();
        let heard = !sys::stderr_is_devnull();
        if let Some((dir, s)) = in_state {
            if s.refusal.as_deref() == Some(w) || !heard {
                return 1;
            }
            say(cause);
            s.refusal = Some(w.to_owned());
            if state::write_file(dir, &s.to_json()).is_err() {
                self.write_note(w);
            }
            return 1;
        }
        let recorded = match (&self.root, &self.key) {
            (Some(root), Some(key)) => state::read_note(root, key),
            _ => None,
        };
        if heard && recorded.as_deref() != Some(w) {
            say(cause);
        }
        // Written every time, also when unchanged: its time tells `get --refresh` that a fetch
        // just refused, so that it waits for the next boundary instead of spawning again.
        let content = match (heard, recorded) {
            (true, _) => w.to_owned(),
            (false, Some(r)) => r,
            (false, None) => "unsaid".to_owned(),
        };
        self.write_note(&content);
        1
    }

    fn write_note(&self, w: &str) {
        if let (Some(root), Some(key)) = (&self.root, &self.key) {
            state::write_note(root, key, w);
        }
    }

    fn clear(&self) {
        if let (Some(root), Some(key)) = (&self.root, &self.key) {
            state::clear_note(root, key);
        }
    }
}

/// A fixed line, never a value from a file, the server or curl.
fn say(cause: Cause) {
    let _ = std::io::stderr()
        .lock()
        .write_all(format!("<4>claude-usage: fetch refused: {}\n", cause.text()).as_bytes());
}

pub fn run(now_flag: bool) -> u8 {
    sys::deadline(DEADLINE);
    let mut note = Note::default();
    let euid = sys::euid();
    if euid == 0 {
        return note.refuse(Cause::Root, None);
    }
    if sys::ruid() != euid {
        return note.refuse(Cause::Setuid, None);
    }
    let home = paths::home();
    let cfg = match paths::config_dir(home.as_deref()) {
        Ok(c) => c,
        Err(ConfigError::Relative) => return note.refuse(Cause::Config, None),
        Err(ConfigError::NoHome) => return note.refuse(Cause::Home, None),
    };
    let Some(home) = home.filter(|h| sys::stat(h).is_ok_and(|st| st.is_dir() && st.uid == euid))
    else {
        return note.refuse(Cause::Home, None);
    };
    let key = paths::key(&cfg);
    note.key = Some(key);
    // Its own binary gone (an uninstall in progress): no new state folder.
    let create = !matches!(paths::exe(), Exe::Deleted);
    let folder = state::open_folder(
        &home,
        note.key.as_deref().unwrap_or(""),
        Access::Write { create },
    );
    note.root = folder.root;
    let dir = match folder.dir {
        Ok(d) => d,
        Err(Problem::Missing) if !create => return 0,
        Err(_) => return note.refuse(Cause::State, None),
    };
    let Some(lockf) = lock::open(&dir) else {
        return note.refuse(Cause::State, None);
    };
    let wait = if now_flag {
        Duration::from_secs(15)
    } else {
        Duration::from_millis(100) // a reader's brief LOCK_SH must not make the timer lose
    };
    match lock::acquire(&lockf, wait) {
        lock::Taken::Yes => {}
        lock::Taken::Busy => return if now_flag { 3 } else { 0 },
        lock::Taken::Failed => return note.refuse(Cause::Lock, None),
    }
    sys::deadline(DEADLINE);
    let now = crate::now();
    lock::record(&lockf, std::process::id(), now);
    state::sweep_temps(&dir, now);

    let loaded = match state::read_file(&dir) {
        Err(_) => return note.refuse(Cause::State, None),
        Ok(None) => Loaded::Empty,
        Ok(Some(b)) => state::parse(&b),
    };
    let config_dir = cfg.to_string_lossy().into_owned();
    let mut st = match loaded {
        Loaded::Full(s) => *s,
        other => {
            let sched = match other {
                Loaded::Kept(k) => k,
                _ => Sched::default(),
            };
            let Ok(salt) = state::new_salt() else {
                return note.refuse(Cause::Write, None);
            };
            State::new(config_dir.clone(), salt, sched)
        }
    };
    st.config_dir = config_dir;

    let creds = creds::read(&cfg, &st.salt);
    let (stamp, fp) = match &creds {
        Creds::Good(g) => (Some(g.stamp.clone()), Some(g.fingerprint.clone())),
        Creds::NoOauth { stamp } => (Some(stamp.clone()), None),
        Creds::Bad { stamp } => (stamp.clone(), None),
        Creds::Missing => (None, None),
        Creds::NotOwned => return note.refuse(Cause::Credentials, Some((&dir, &mut st))),
    };
    let mut dirty = st.refusal.take().is_some();
    if let (Some(stamp), Some(fp)) = (&stamp, &fp) {
        // The same claudeAiOauth under a new stamp (an MCP login, a chmod, a backup tool): the
        // numbers stay, with no call.
        if st.fetched_at.is_some()
            && st.numbers_stamp.is_some()
            && st.numbers_stamp.as_ref() != Some(stamp)
            && st.numbers_fingerprint.as_ref() == Some(fp)
        {
            st.numbers_stamp = Some(stamp.clone());
            dirty = true;
        }
    }
    // This file was looked at and needs no call now (the same fingerprint, or new credentials
    // during a 429 backoff, which they do not end): `get --refresh` stops counting its stamp as
    // new. The fingerprint stays, so new credentials are still called once the backoff ends.
    if let (Some(stamp), Some(fp)) = (&stamp, &fp)
        && st.attempt_stamp.as_ref() != Some(stamp)
        && (st.attempt_fingerprint.as_ref() == Some(fp) || schedule::rate_limited(&st.sched, now))
    {
        st.attempt_stamp = Some(stamp.clone());
        dirty = true;
    }
    let changed = match &fp {
        Some(fp) => st.attempt_fingerprint.as_ref() != Some(fp),
        None => st.attempt_stamp != stamp,
    };
    let mut slot = false;
    if !schedule::due(&st.sched, now, changed) {
        if now_flag && schedule::now_allowed(&st.sched, now) {
            slot = true;
        } else {
            if dirty && !write(&dir, &st, &note) {
                return note.refuse(Cause::Write, None);
            }
            return if now_flag { 3 } else { 0 };
        }
    }

    let no_call = match &creds {
        Creds::Missing | Creds::NoOauth { .. } => Some(Outcome::NoCredentials),
        Creds::Bad { .. } | Creds::NotOwned => Some(Outcome::BadCredentials),
        Creds::Good(g) if g.expired(now) => Some(Outcome::TokenExpired),
        Creds::Good(_) => None,
    };
    let curl = match no_call {
        None => curl::find(config::read(&home).curl.as_deref()),
        Some(_) => None,
    };
    st.sched.last_attempt = Some(now);
    st.sched.next_attempt = None; // used up by this attempt; steps and streaks stay
    st.attempt_stamp = stamp.clone();
    st.attempt_fingerprint = fp.clone();
    let (Creds::Good(good), Some(curl)) = (&creds, curl) else {
        // No call. Recorded like an attempt, so that it is checked again on new credentials or
        // at the next boundary, not on every run.
        st.pending = false;
        schedule::apply(&mut st.sched, no_call.unwrap_or(Outcome::NoCurl), now);
        return save(&dir, &st, &note);
    };

    // The write-ahead record. If it cannot be written, no call: this is what stops a retry
    // storm when fetches are killed mid-call.
    st.pending = true;
    if slot {
        st.sched.now_times.push(now);
    }
    schedule::prune_now_times(&mut st.sched, now);
    if !write(&dir, &st, &note) {
        return note.refuse(Cause::Write, None);
    }
    let ended = curl::call(&curl, &good.header);
    drop(creds); // zeroes the token line (the read buffer was zeroed in creds::read)
    let done = crate::now();
    let (outcome, windows) = outcome_of(ended, done);
    st.pending = false;
    if let Some(w) = windows {
        // Under the stamp of the file the token came from, not the file now.
        st.fetched_at = Some(done);
        st.windows = w;
        st.numbers_stamp = stamp;
        st.numbers_fingerprint = fp;
    }
    schedule::apply(&mut st.sched, outcome, now);
    save(&dir, &st, &note)
}

/// Writes the state; a write that worked ends any refusal recorded beside the folder.
fn write(dir: &OwnedFd, st: &State, note: &Note) -> bool {
    let ok = state::write_file(dir, &st.to_json()).is_ok();
    if ok {
        note.clear();
    }
    ok
}

fn save(dir: &OwnedFd, st: &State, note: &Note) -> u8 {
    if write(dir, st, note) {
        0
    } else {
        note.refuse(Cause::Write, None)
    }
}

fn outcome_of(e: Ended, now: i64) -> (Outcome, Option<Windows>) {
    match e {
        Ended::NoCurl => (Outcome::NoCurl, None),
        Ended::Network => (Outcome::Network, None),
        Ended::TooBig => (Outcome::BadResponse, None),
        Ended::Response {
            status,
            body,
            retry_after,
        } => match status {
            200 => match parse::usage(&body, now) {
                Some(w) => (Outcome::Success, Some(w)),
                None => (Outcome::BadResponse, None),
            },
            429 => (
                Outcome::RateLimited {
                    until: parse::retry_after(&retry_after, now).map(|r| now.saturating_add(r)),
                    seen: !retry_after.trim().is_empty(),
                },
                None,
            ),
            401 => (Outcome::Unauthorized, None),
            403 => (Outcome::Forbidden, None),
            500..=599 => (Outcome::ServerError, None),
            _ => (Outcome::OtherStatus, None),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, body: &str, ra: &str) -> Ended {
        Ended::Response {
            status,
            body: body.as_bytes().to_vec(),
            retry_after: ra.into(),
        }
    }

    #[test]
    fn retry_after_counts_from_the_response() {
        let done = 1_794_893_100;
        assert_eq!(
            outcome_of(resp(429, "", "120"), done).0,
            Outcome::RateLimited {
                until: Some(done + 120),
                seen: true
            }
        );
        assert_eq!(
            outcome_of(resp(429, "", "Tue, 17 Nov 2026 05:30:00 GMT"), done).0,
            Outcome::RateLimited {
                until: Some(done + 300),
                seen: true
            }
        );
    }

    #[test]
    fn statuses() {
        let o = |e| outcome_of(e, 1_794_893_100).0;
        assert_eq!(
            o(resp(200, r#"{"five_hour":null,"seven_day":null}"#, "")),
            Outcome::Success
        );
        assert_eq!(o(resp(200, "<html>", "")), Outcome::BadResponse);
        assert_eq!(
            o(resp(429, "", "")),
            Outcome::RateLimited {
                until: None,
                seen: false
            }
        );
        assert_eq!(
            o(resp(429, "", "soon")),
            Outcome::RateLimited {
                until: None,
                seen: true
            }
        );
        assert_eq!(o(resp(401, "", "")), Outcome::Unauthorized);
        assert_eq!(o(resp(403, "", "")), Outcome::Forbidden);
        assert_eq!(o(resp(503, "", "")), Outcome::ServerError);
        assert_eq!(o(resp(404, "", "")), Outcome::OtherStatus);
        assert_eq!(o(resp(302, "", "")), Outcome::OtherStatus);
        assert_eq!(o(Ended::TooBig), Outcome::BadResponse);
        assert_eq!(o(Ended::Network), Outcome::Network);
        assert_eq!(o(Ended::NoCurl), Outcome::NoCurl);
    }
}
