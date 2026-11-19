//! `status`: the state in words, for a person. Not a contract. Reads only; never opens the
//! credentials (a `stat`), never touches the network.

use crate::config;
use crate::creds::{self, Seen};
use crate::curl;
use crate::get;
use crate::lock::{self, Holder};
use crate::parse::civil_from_days;
use crate::paths::{self, ConfigError};
use crate::schedule;
use crate::state::{self, Access, Loaded, Problem};
use crate::sys;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

fn when(t: i64, now: i64) -> String {
    // Years 1970 to 9999; anything else (a corrupt state) as the raw number.
    if !(0..=253_402_300_799).contains(&t) {
        return format!("{t} (not a valid time)");
    }
    let (y, m, d) = civil_from_days(t.div_euclid(86_400));
    let s = t.rem_euclid(86_400);
    let rel = if t <= now {
        format!("{} s ago", now.saturating_sub(t))
    } else {
        format!("in {} s", t.saturating_sub(now))
    };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC ({rel})",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

pub fn run() -> u8 {
    let mut o = String::new();
    report(&mut o, crate::now());
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(o.as_bytes());
    let _ = out.flush();
    0
}

fn report(o: &mut String, now: i64) {
    let _ = writeln!(o, "claude-usage {}", env!("CARGO_PKG_VERSION"));
    let euid = sys::euid();
    let home = paths::home();
    let mut warnings = Vec::new();
    match (&home, sys::passwd_home(euid)) {
        (_, None) => warnings.push(format!(
            "uid {euid} has no passwd entry: the systemd timer may use another home"
        )),
        (Some(h), Some(p)) if paths::canonical(h) != paths::canonical(&p) => warnings.push(format!(
            "HOME ({}) is not your passwd home ({}): the timer uses the passwd home, so the two keep separate state",
            h.display(),
            p.display()
        )),
        _ => {}
    }
    let cfg = match paths::config_dir(home.as_deref()) {
        Ok(c) => c,
        Err(ConfigError::Relative) => {
            let _ = writeln!(
                o,
                "config folder:  CLAUDE_CONFIG_DIR is relative (bad_config)"
            );
            return finish(o, &warnings);
        }
        Err(ConfigError::NoHome) => {
            let _ = writeln!(o, "config folder:  none (HOME is not set or not absolute)");
            return finish(o, &warnings);
        }
    };
    let _ = writeln!(o, "config folder:  {}", cfg.display());
    let seen = creds::look(&cfg);
    let _ = writeln!(
        o,
        "credentials:    {}",
        match &seen {
            Seen::Missing => "missing (not logged in, or an API-key setup)".to_owned(),
            Seen::Present { uid, .. } if *uid != euid =>
                "present, NOT yours (fetch refuses)".into(),
            Seen::Present { .. } => "present".into(),
            Seen::Unknown => "cannot be checked".into(),
        }
    );
    let Some(home) = home else {
        return finish(o, &warnings);
    };
    let key = paths::key(&cfg);
    let _ = writeln!(
        o,
        "state folder:   {}",
        home.join(".local/state/claude-usage").join(&key).display()
    );
    let folder = state::open_folder(&home, &key, Access::Look);
    if let Some(note) = folder.root.as_ref().and_then(|r| state::read_note(r, &key)) {
        let _ = writeln!(o, "refused:        {note} (the last fetch refused to run)");
    }
    let dir = match folder.dir {
        Ok(d) => d,
        Err(Problem::Missing) => {
            let _ = writeln!(o, "state:          none yet (no fetch has run)");
            return finish(o, &warnings);
        }
        Err(Problem::Unusable) => {
            let _ = writeln!(
                o,
                "state:          UNUSABLE (a symlink, not a folder, or not yours)"
            );
            return finish(o, &warnings);
        }
    };
    let holder = lock::test(&dir);
    let contents = state::read_file(&dir);
    let lock_free = matches!(holder, Holder::Free(_));
    let lock_bad = matches!(holder, Holder::Bad);
    drop(holder);
    let loaded = match contents {
        Err(_) => {
            let _ = writeln!(
                o,
                "state:          UNUSABLE (usage.json is not a regular file of yours)"
            );
            return finish(o, &warnings);
        }
        Ok(None) => Loaded::Empty,
        Ok(Some(b)) => state::parse(&b),
    };
    let s = match &loaded {
        Loaded::Full(s) => s,
        Loaded::Kept(_) => {
            let _ = writeln!(
                o,
                "state:          another version (the next fetch rewrites it)"
            );
            return finish(o, &warnings);
        }
        Loaded::Empty => {
            let _ = writeln!(o, "state:          empty");
            return finish(o, &warnings);
        }
    };
    let shown = get::evaluate(Some(s), &seen, lock_free, now);
    let _ = writeln!(
        o,
        "get shows:      error {}, numbers {}",
        shown.error.as_deref().unwrap_or("none"),
        if shown.fetched_at.is_some() {
            "shown"
        } else {
            "hidden"
        }
    );
    match s.fetched_at {
        Some(t) => {
            let _ = writeln!(o, "last fetched:   {}", when(t, now));
        }
        None => {
            let _ = writeln!(o, "last fetched:   never");
        }
    }
    if let Some(t) = s.sched.last_attempt {
        let _ = writeln!(
            o,
            "last attempt:   {}, {}{}",
            when(t, now),
            s.sched.last_error.as_deref().unwrap_or("ok"),
            if s.pending { " (did not finish)" } else { "" }
        );
    }
    let next = match schedule::wait_until(&s.sched, now) {
        Some(t) => format!("not before {}", when(t, now)),
        None => format!("at the next :00/:30 (in {} s)", 30 - now.rem_euclid(30)),
    };
    let _ = writeln!(o, "next fetch:     {next}");
    if s.sched.backoff_step > 0 || s.sched.fail_streak > 0 {
        let _ = writeln!(
            o,
            "backoff:        step {}, failures in a row {}",
            s.sched.backoff_step, s.sched.fail_streak
        );
    }
    let mut now_s = s.sched.clone();
    schedule::prune_now_times(&mut now_s, now);
    if !now_s.now_times.is_empty() {
        let _ = writeln!(
            o,
            "--now used:     {} in the last 10 minutes",
            now_s.now_times.len()
        );
    }
    let _ = writeln!(
        o,
        "lock:           {}",
        match (lock_bad, lock_free, lock::recorded(&dir)) {
            (true, _, _) =>
                "UNUSABLE (not a regular file of yours, or cannot be locked)".to_owned(),
            (_, true, _) => "free".to_owned(),
            (_, false, Some((pid, since))) =>
                format!("held by pid {pid} since {}", when(since, now)),
            (_, false, None) => "held".to_owned(),
        }
    );
    if let Some(r) = &s.refusal {
        let _ = writeln!(o, "refused:        {r} (the last fetch refused to run)");
    }
    let _ = writeln!(
        o,
        "curl:           {}",
        curl::find(config::read(&home).curl.as_deref())
            .map_or("none usable (no_curl)".to_owned(), |p| p
                .display()
                .to_string())
    );
    let wants = home.join(".config/systemd/user/timers.target.wants/claude-usage.timer");
    let _ = writeln!(
        o,
        "timer:          {}",
        if Path::new(&wants).exists() {
            "enabled"
        } else {
            "not enabled (readers' get --refresh still start fetches)"
        }
    );
    finish(o, &warnings);
}

fn finish(o: &mut String, warnings: &[String]) {
    for w in warnings {
        let _ = writeln!(o, "warning:        {w}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(
            when(1_794_893_100, 1_794_893_160),
            "2026-11-17 05:25:00 UTC (60 s ago)"
        );
        assert_eq!(when(10, 0), "1970-01-01 00:00:10 UTC (in 10 s)");
    }
}
