//! `get [--refresh]`: the cached numbers as JSON. Never touches the network, never writes,
//! only `stat`s the credentials. Always prints JSON and exits 0.

use crate::creds::{self, Seen};
use crate::detach;
use crate::lock::{self, Holder};
use crate::parse::{Window, Windows};
use crate::paths::{self, ConfigError, Exe};
use crate::schedule;
use crate::state::{self, Access, Loaded, Problem, Sched, State};
use crate::sys;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What `get` shows.
#[derive(Debug, PartialEq)]
pub struct Shown {
    pub fetched_at: Option<i64>,
    pub error: Option<String>,
    pub windows: Windows,
}

impl Shown {
    fn hidden(word: &str) -> Shown {
        Shown {
            fetched_at: None,
            error: Some(word.to_owned()),
            windows: Windows::default(),
        }
    }

    pub fn render(&self) -> String {
        format!(
            "{{\"schema\":0,\"fetched_at\":{},\"error\":{},\"windows\":{{\"five_hour\":{},\"seven_day\":{}}}}}\n",
            self.fetched_at.map_or(Value::Null, Value::from),
            self.error.clone().map_or(Value::Null, Value::String),
            state::window_json(&self.windows.five_hour),
            state::window_json(&self.windows.seven_day),
        )
    }
}

/// The view of a usable state folder. `creds`: the credentials file's `stat`. `lock_free`: no
/// fetch holds the lock (then the state read is quiescent).
pub fn evaluate(state: Option<&State>, creds: &Seen, lock_free: bool, now: i64) -> Shown {
    if matches!(creds, Seen::Missing) {
        return Shown::hidden("no_credentials");
    }
    let Some(s) = state else {
        return Shown {
            fetched_at: None,
            error: None,
            windows: Windows::default(),
        };
    };
    let has_numbers = s.fetched_at.is_some() && s.numbers_stamp.is_some();
    let current = match creds {
        Seen::Present { stamp, .. } => Some(stamp.as_str()),
        _ => None,
    };
    let visible = has_numbers && current == s.numbers_stamp.as_deref();
    // Never another account's numbers, nor numbers no fetch has confirmed for this file.
    if has_numbers && !visible {
        return Shown::hidden("account_changed");
    }
    let stalled = if lock_free {
        s.pending
    } else {
        s.sched
            .last_attempt
            .is_some_and(|la| now.saturating_sub(la) > 60)
    };
    Shown {
        fetched_at: if visible { s.fetched_at } else { None },
        error: if stalled {
            Some("stalled".into())
        } else {
            s.sched.last_error.clone()
        },
        windows: if visible {
            Windows {
                five_hour: current_window(s.windows.five_hour.as_ref(), now),
                seven_day: current_window(s.windows.seven_day.as_ref(), now),
            }
        } else {
            Windows::default()
        },
    }
}

/// A window whose reset time has come is shown as `null`: its numbers belong to a window that
/// has ended. One that has not started (`resets_at` null) is shown as it is.
fn current_window(w: Option<&Window>, now: i64) -> Option<Window> {
    w.filter(|w| w.resets_at.is_none_or(|r| r > now)).cloned()
}

pub fn run(refresh: bool) -> u8 {
    let now = crate::now();
    let home = paths::home();
    let (shown, spawn) = match paths::config_dir(home.as_deref()) {
        Err(ConfigError::Relative) => (Shown::hidden("bad_config"), None),
        Err(ConfigError::NoHome) => (Shown::hidden("state_unusable"), None),
        Ok(cfg) => match home {
            Some(h) => look(&h, &cfg, now, refresh),
            None => (Shown::hidden("state_unusable"), None),
        },
    };
    // A reader that went away is not an error: the write error is dropped.
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(shown.render().as_bytes());
    let _ = out.flush();
    drop(out);
    if let Some((exe, cfg)) = spawn {
        detach::spawn_fetch(&exe, &cfg);
    }
    0
}

/// The result, and what to spawn (`--refresh` only): the program and the config folder.
fn look(home: &Path, cfg: &Path, now: i64, refresh: bool) -> (Shown, Option<(PathBuf, PathBuf)>) {
    let key = paths::key(cfg);
    let folder = state::open_folder(home, &key, Access::Look);
    let dir = match folder.dir {
        Ok(d) => Some(d),
        Err(Problem::Missing) => None,
        Err(Problem::Unusable) => return (Shown::hidden("state_unusable"), None),
    };
    // A refusal in this boundary window (the note's time): a fetch now would refuse again.
    let refused_now = folder
        .root
        .as_ref()
        .and_then(|r| state::note_time(r, &key))
        .is_some_and(|t| t.div_euclid(30) == now.div_euclid(30));
    // A fetch that is not due holds the lock for a few ms; a reader that meets it then with an
    // old last attempt would say `stalled` for nothing, so such a meeting is retried briefly.
    let mut tries = 0;
    loop {
        let holder = dir.as_ref().map_or(Holder::Free(None), lock::test);
        if matches!(holder, Holder::Bad) {
            return (Shown::hidden("state_unusable"), None);
        }
        let contents = match &dir {
            Some(d) => state::read_file(d),
            None => Ok(None),
        };
        let lock_free = matches!(holder, Holder::Free(_));
        drop(holder);
        let loaded = match contents {
            Err(_) => return (Shown::hidden("state_unusable"), None),
            Ok(None) => Loaded::Empty,
            Ok(Some(b)) => state::parse(&b),
        };
        let seen = creds::look(cfg);
        let full = match &loaded {
            Loaded::Full(s) => Some(s.as_ref()),
            _ => None,
        };
        let shown = evaluate(full, &seen, lock_free, now);
        tries += 1;
        if !lock_free && shown.error.as_deref() == Some("stalled") && tries < 30 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let spawn = refresh
            && lock_free
            && !refused_now
            && may_spawn(home, &seen)
            && spawn_due(&loaded, &seen, now);
        let exe = match paths::exe() {
            Exe::Path(p) if spawn => Some(p),
            _ => None,
        };
        return (shown, exe.map(|e| (e, cfg.to_path_buf())));
    }
}

/// Who may run a writer (design section 5): not root, no setuid, `$HOME` and the credentials
/// file the user's. (The state folder's owner was checked when it was opened.) No credentials
/// file: nothing to fetch.
fn may_spawn(home: &Path, seen: &Seen) -> bool {
    let euid = sys::euid();
    !matches!(seen, Seen::Missing)
        && euid != 0
        && sys::ruid() == euid
        && sys::stat(home).is_ok_and(|st| st.is_dir() && st.uid == euid)
        && !matches!(seen, Seen::Present { uid, .. } if *uid != euid)
}

/// Whether a fetch has work now, judged from a `stat`: a call is due, or the file has a stamp no
/// fetch has looked at (a new fingerprint, or the same one to re-stamp the numbers with; the
/// fetch decides, also during a 429 backoff). Never within the 5 s floor.
fn spawn_due(loaded: &Loaded, seen: &Seen, now: i64) -> bool {
    let empty = Sched::default();
    let (sched, attempt_stamp) = match loaded {
        Loaded::Full(s) => (&s.sched, s.attempt_stamp.as_deref()),
        Loaded::Kept(k) => (k, None),
        Loaded::Empty => (&empty, None),
    };
    let current = match seen {
        Seen::Present { stamp, .. } => Some(stamp.as_str()),
        _ => None,
    };
    let changed = current != attempt_stamp;
    schedule::due(sched, now, changed) || (changed && schedule::past_floor(sched, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_893_100;

    fn with_numbers() -> State {
        let mut s = State::new("/c".into(), "0".repeat(32), Sched::default());
        s.fetched_at = Some(NOW - 100);
        s.numbers_stamp = Some("1:2:3:4:5".into());
        s.attempt_stamp = s.numbers_stamp.clone();
        s.sched.last_attempt = Some(NOW - 100);
        s.windows.five_hour = Some(Window {
            pct: 12.into(),
            resets_at: None,
        });
        s
    }

    fn at(stamp: &str) -> Seen {
        Seen::Present {
            stamp: stamp.into(),
            uid: 0,
        }
    }

    #[test]
    fn numbers_shown_only_under_their_stamp() {
        let s = with_numbers();
        let v = evaluate(Some(&s), &at("1:2:3:4:5"), true, NOW);
        assert_eq!(v.fetched_at, Some(NOW - 100));
        assert_eq!(v.error, None);
        let v = evaluate(Some(&s), &at("9:9:9:9:9"), true, NOW);
        assert_eq!(v, Shown::hidden("account_changed"));
        let v = evaluate(Some(&s), &Seen::Unknown, true, NOW);
        assert_eq!(v, Shown::hidden("account_changed"));
    }

    #[test]
    fn precedence() {
        let mut s = with_numbers();
        s.sched.last_error = Some("rate_limited".into());
        s.pending = true;
        // no_credentials over account_changed and stalled
        assert_eq!(
            evaluate(Some(&s), &Seen::Missing, true, NOW),
            Shown::hidden("no_credentials")
        );
        // account_changed over stalled
        assert_eq!(
            evaluate(Some(&s), &at("x:0:0:0:0"), true, NOW),
            Shown::hidden("account_changed")
        );
        // stalled over the kept error, numbers kept
        let v = evaluate(Some(&s), &at("1:2:3:4:5"), true, NOW);
        assert_eq!(v.error.as_deref(), Some("stalled"));
        assert!(v.windows.five_hour.is_some());
        // a fetch in progress with a fresh attempt: the kept error
        s.sched.last_attempt = Some(NOW - 2);
        let v = evaluate(Some(&s), &at("1:2:3:4:5"), false, NOW);
        assert_eq!(v.error.as_deref(), Some("rate_limited"));
        // the lock held over 60 s after the attempt: stalled
        s.sched.last_attempt = Some(NOW - 61);
        s.pending = false;
        let v = evaluate(Some(&s), &at("1:2:3:4:5"), false, NOW);
        assert_eq!(v.error.as_deref(), Some("stalled"));
        s.sched.last_attempt = Some(NOW - 60);
        let v = evaluate(Some(&s), &at("1:2:3:4:5"), false, NOW);
        assert_eq!(
            v.error.as_deref(),
            Some("rate_limited"),
            "exactly 60 s: not yet"
        );
        // no state at all: no_credentials when the file is gone, else the empty result
        assert_eq!(
            evaluate(None, &Seen::Missing, true, NOW),
            Shown::hidden("no_credentials")
        );
    }

    #[test]
    fn no_numbers_show_the_recorded_error() {
        let mut s = State::new("/c".into(), "0".repeat(32), Sched::default());
        s.sched.last_error = Some("no_credentials".into());
        let v = evaluate(Some(&s), &at("1:1:1:1:1"), true, NOW);
        assert_eq!(v, Shown::hidden("no_credentials"));
        assert_eq!(evaluate(None, &at("1:1:1:1:1"), true, NOW).error, None);
    }

    #[test]
    fn a_window_that_has_reset_is_null() {
        let w = |resets_at| {
            Some(Window {
                pct: 12.into(),
                resets_at,
            })
        };
        let mut s = with_numbers();
        let shown = |s: &State| evaluate(Some(s), &at("1:2:3:4:5"), true, NOW).windows;
        // Not started: shown as it is.
        assert_eq!(shown(&s).five_hour, w(None));
        s.windows.five_hour = w(Some(NOW + 1));
        assert_eq!(shown(&s).five_hour, w(Some(NOW + 1)));
        // Its reset time has come: null, at the second and after.
        s.windows.five_hour = w(Some(NOW));
        assert_eq!(shown(&s).five_hour, None);
        s.windows.five_hour = w(Some(NOW - 3600));
        assert_eq!(shown(&s).five_hour, None);
        // Per window; fetched_at and the error stay as they are.
        s.windows.seven_day = w(Some(NOW + 86_400));
        s.sched.last_error = Some("token_expired".into());
        let v = evaluate(Some(&s), &at("1:2:3:4:5"), true, NOW);
        assert_eq!(v.fetched_at, Some(NOW - 100));
        assert_eq!(v.error.as_deref(), Some("token_expired"));
        assert_eq!(v.windows.five_hour, None);
        assert_eq!(v.windows.seven_day, w(Some(NOW + 86_400)));
        s.windows.seven_day = w(Some(NOW));
        assert_eq!(shown(&s), Windows::default());
    }

    #[test]
    fn render_is_the_contract() {
        let mut v = Shown::hidden("rate_limited");
        assert_eq!(
            v.render(),
            "{\"schema\":0,\"fetched_at\":null,\"error\":\"rate_limited\",\"windows\":{\"five_hour\":null,\"seven_day\":null}}\n"
        );
        v.error = None;
        v.fetched_at = Some(NOW);
        v.windows.seven_day = Some(Window {
            pct: serde_json::from_str("30.4").expect("n"),
            resets_at: Some(NOW + 1),
        });
        assert_eq!(
            v.render(),
            format!(
                "{{\"schema\":0,\"fetched_at\":{NOW},\"error\":null,\"windows\":{{\"five_hour\":null,\"seven_day\":{{\"pct\":30.4,\"resets_at\":{}}}}}}}\n",
                NOW + 1
            )
        );
    }
}
