//! When a call is due, the `--now` limits, and what each result does to the schedule (design
//! section 5, When due). Pure functions of the state and an injected clock.

use crate::state::Sched;

/// No call within this many seconds of the last attempt, ever.
pub const FLOOR: i64 = 5;
/// `--now`: at least this long since the last attempt.
const NOW_GAP: i64 = 10;
/// `--now`: at most this many per sliding window of this length.
const NOW_MAX: usize = 6;
const NOW_SPAN: i64 = 600;
/// The longest any wait lasts: a `next_attempt` further than this after the last attempt is
/// capped (a corrupt or clock-jumped value).
const MAX_WAIT: i64 = 3600;
/// No call could be made (no curl, no or bad credentials): looked at again after this, or at
/// once when the credentials file changes. Keeps a logged-out timer from writing every 30 s.
const NO_CALL_HOLD: i64 = 300;
/// A `last_attempt` further than this in the future is invalid.
const FUTURE_SLACK: i64 = 60;

/// The result of one attempt, with or without a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// 429; `until`: the time the `Retry-After` header names (from the response), when present
    /// and usable; `seen`:
    /// whether the response carried the header at all.
    RateLimited {
        until: Option<i64>,
        seen: bool,
    },
    Unauthorized,
    Forbidden,
    /// 5xx.
    ServerError,
    /// Connection failed or timed out.
    Network,
    /// Any other non-200 status.
    OtherStatus,
    BadResponse,
    NoCurl,
    NoCredentials,
    BadCredentials,
    TokenExpired,
}

impl Outcome {
    /// The error word recorded in the state (and shown by `get`).
    pub fn word(self) -> Option<&'static str> {
        Some(match self {
            Outcome::Success => return None,
            Outcome::RateLimited { .. } => "rate_limited",
            Outcome::Unauthorized => "unauthorized",
            Outcome::Forbidden => "forbidden",
            Outcome::ServerError | Outcome::OtherStatus => "http_error",
            Outcome::Network => "network",
            Outcome::BadResponse => "bad_response",
            Outcome::NoCurl => "no_curl",
            Outcome::NoCredentials => "no_credentials",
            Outcome::BadCredentials => "bad_credentials",
            Outcome::TokenExpired => "token_expired",
        })
    }
}

fn window(t: i64) -> i64 {
    t.div_euclid(30)
}

/// The last attempt, unless it lies more than 60 s in the future.
fn last(s: &Sched, now: i64) -> Option<i64> {
    s.last_attempt
        .filter(|&la| la <= now.saturating_add(FUTURE_SLACK))
}

/// The end of the current backoff or hold, capped at 1 h after the last attempt. Without a
/// valid last attempt (a state of another version), a `next_attempt` within the hour ahead is
/// honoured as it is, and a later one ignored.
pub fn wait_until(s: &Sched, now: i64) -> Option<i64> {
    let na = s.next_attempt?;
    match last(s, now) {
        Some(la) => Some(na.min(la.saturating_add(MAX_WAIT))),
        None => (na <= now.saturating_add(MAX_WAIT)).then_some(na),
    }
}

/// The hold of an attempt that could make no call.
fn no_call_hold(s: &Sched) -> bool {
    matches!(
        s.last_error.as_deref(),
        Some("no_curl" | "no_credentials" | "bad_credentials")
    )
}

/// Past the 5 s floor (or no valid last attempt).
pub fn past_floor(s: &Sched, now: i64) -> bool {
    last(s, now).is_none_or(|la| now >= la.saturating_add(FLOOR))
}

/// Under a 429 backoff, which new credentials do not end.
pub fn rate_limited(s: &Sched, now: i64) -> bool {
    s.last_error.as_deref() == Some("rate_limited") && wait_until(s, now).is_some_and(|na| now < na)
}

/// Whether a call is due. `changed`: the credentials changed since the last attempt (another
/// fingerprint; for unreadable credentials, another stamp).
pub fn due(s: &Sched, now: i64, changed: bool) -> bool {
    if !past_floor(s, now) {
        return false;
    }
    match (wait_until(s, now), last(s, now)) {
        // Under a backoff or hold: its end, or new credentials, except during a 429 backoff.
        (Some(na), _) => now >= na || (changed && s.last_error.as_deref() != Some("rate_limited")),
        // Normal: a :00/:30 boundary since the last attempt, or new credentials.
        (None, Some(la)) => changed || window(now) > window(la),
        (None, None) => true,
    }
}

/// Whether `fetch --now` may call when a call is not otherwise due.
pub fn now_allowed(s: &Sched, now: i64) -> bool {
    if let Some(la) = last(s, now) {
        if now < la.saturating_add(NOW_GAP) {
            return false;
        }
        // A no-call hold (no curl, no or bad credentials) made no call to space out: `--now`
        // may look again at once, after the user fixed it.
        if wait_until(s, now).is_some_and(|na| now < na) && !no_call_hold(s) {
            return false;
        }
    }
    let recent: Vec<i64> = live_now_times(s, now).collect();
    !recent.iter().any(|&t| window(t) == window(now)) && recent.len() < NOW_MAX
}

fn live_now_times(s: &Sched, now: i64) -> impl Iterator<Item = i64> + '_ {
    s.now_times
        .iter()
        .copied()
        .filter(move |&t| t > now - NOW_SPAN && t <= now.saturating_add(FUTURE_SLACK))
}

/// Drops `--now` times that no longer count.
pub fn prune_now_times(s: &mut Sched, now: i64) {
    s.now_times = live_now_times(s, now).collect();
}

/// 60 s doubled `step - 1` times, capped.
fn doubling(step: u32, cap: i64) -> i64 {
    let shift = step.saturating_sub(1).min(20);
    (60_i64 << shift).min(cap)
}

/// Records an outcome. `la`: the attempt's time (already in `s.last_attempt`); `next_attempt`
/// was cleared by the write-ahead record.
pub fn apply(s: &mut Sched, outcome: Outcome, la: i64) {
    // Each doubling backoff (429; other statuses and bad responses) counts results of its own
    // kind in a row: any other call result in between starts it again at 60 s. Results without
    // a call, and a killed attempt (it leaves `last_error` as it was), break no run.
    let prev = s.last_error.as_deref();
    let neutral = matches!(
        prev,
        None | Some("no_curl" | "no_credentials" | "bad_credentials" | "token_expired")
    );
    let prev_rate = prev == Some("rate_limited");
    let prev_doubling = match prev {
        Some("bad_response") => true,
        Some("http_error") => s.fail_streak == 0, // with a streak it was a 5xx
        _ => false,
    };
    s.last_error = outcome.word().map(str::to_owned);
    s.next_attempt = None;
    match outcome {
        Outcome::Success => {
            s.backoff_step = 0;
            s.fail_streak = 0;
        }
        Outcome::RateLimited { until, seen } => {
            if !(prev_rate || neutral) {
                s.backoff_step = 0;
            }
            s.backoff_step = s.backoff_step.saturating_add(1).min(64);
            s.fail_streak = 0;
            // The step from the attempt, or the server's time when later; at most 1 h.
            let step = la.saturating_add(doubling(s.backoff_step, 600));
            let next = match until {
                Some(u) if u > step => u.min(la.saturating_add(MAX_WAIT)),
                _ => step,
            };
            s.next_attempt = Some(next);
            s.retry_after_seen = Some(seen);
        }
        Outcome::Unauthorized | Outcome::Forbidden => {
            s.fail_streak = 0;
            s.next_attempt = Some(la.saturating_add(MAX_WAIT));
        }
        Outcome::ServerError | Outcome::Network => {
            s.fail_streak = s.fail_streak.saturating_add(1).min(64);
            if s.fail_streak >= 3 {
                s.next_attempt = Some(la.saturating_add(doubling(s.fail_streak - 2, 300)));
            }
        }
        Outcome::OtherStatus | Outcome::BadResponse => {
            if !(prev_doubling || neutral) {
                s.backoff_step = 0;
            }
            s.backoff_step = s.backoff_step.saturating_add(1).min(64);
            s.fail_streak = 0;
            s.next_attempt = Some(la.saturating_add(doubling(s.backoff_step, MAX_WAIT)));
        }
        Outcome::TokenExpired => s.next_attempt = Some(la.saturating_add(MAX_WAIT)),
        Outcome::NoCurl | Outcome::NoCredentials | Outcome::BadCredentials => {
            s.next_attempt = Some(la.saturating_add(NO_CALL_HOLD));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: i64 = 1_794_893_100; // a :00 boundary (divisible by 30)

    fn at(la: i64) -> Sched {
        Sched {
            last_attempt: Some(la),
            ..Sched::default()
        }
    }

    #[test]
    fn boundary_and_floor() {
        assert!(due(&Sched::default(), B, false));
        assert!(!due(&at(B + 1), B + 29, false));
        assert!(due(&at(B + 29), B + 34, false));
        assert!(!due(&at(B + 28), B + 31, false), "floor");
        assert!(
            due(&at(B + 28), B + 33, false),
            "the floor ends at exactly 5 s"
        );
        assert!(
            !due(&at(B + 1), B + 3, true),
            "floor holds for new credentials"
        );
        assert!(due(&at(B + 1), B + 6, true));
    }

    #[test]
    fn clock_jumps() {
        // A last attempt over 60 s in the future is invalid: due.
        assert!(due(&at(B + 3600), B, false));
        assert!(due(&at(B + 61), B, false), "61 s ahead: invalid");
        assert!(!due(&at(B + 60), B, false), "60 s ahead: still valid");
        // Within 60 s in the future: the floor and the window hold.
        assert!(!due(&at(B + 20), B, false));
        // The clock jumped back a day: the last attempt is now in the future, invalid, due.
        assert!(due(&at(B), B - 86_400, false));
        // A next_attempt far ahead waits at most an hour.
        let mut s = at(B);
        s.next_attempt = Some(B + 86_400);
        assert!(!due(&s, B + 3599, false));
        assert!(due(&s, B + 3600, false));
    }

    #[test]
    fn holds_and_new_credentials() {
        let mut s = at(B);
        apply(&mut s, Outcome::Unauthorized, B);
        assert_eq!(s.next_attempt, Some(B + 3600));
        assert!(!due(&s, B + 40, false));
        assert!(due(&s, B + 40, true), "a new fingerprint ends a 401 hold");
        let mut s = at(B);
        apply(
            &mut s,
            Outcome::RateLimited {
                until: None,
                seen: false,
            },
            B,
        );
        assert!(!due(&s, B + 40, true), "but not a 429 backoff");
        assert!(due(&s, B + 60, false));
        let mut s = at(B);
        apply(&mut s, Outcome::TokenExpired, B);
        assert_eq!(
            s.next_attempt,
            Some(B + 3600),
            "token_expired: held an hour, as 401"
        );
        assert!(!due(&s, B + 3599, false));
        assert!(due(&s, B + 3600, false));
        assert!(due(&s, B + 40, true));
    }

    fn waits(outcome: Outcome, n: usize) -> Vec<Option<i64>> {
        let mut s = Sched::default();
        (0..n)
            .map(|_| {
                apply(&mut s, outcome, B);
                s.next_attempt.map(|na| na - B)
            })
            .collect()
    }

    #[test]
    fn no_call_hold() {
        for o in [
            Outcome::NoCurl,
            Outcome::NoCredentials,
            Outcome::BadCredentials,
        ] {
            let mut s = at(B);
            apply(&mut s, o, B);
            assert!(!due(&s, B + 40, false), "held past the next boundary");
            assert!(due(&s, B + 40, true), "new credentials end the hold");
            assert!(!due(&s, B + 299, false));
            assert!(due(&s, B + 300, false), "looked at again after 5 min");
            assert!(now_allowed(&s, B + 10), "--now looks again at once");
            assert!(!now_allowed(&s, B + 9), "after the usual 10 s gap");
        }
    }

    #[test]
    fn backoff_tables() {
        let rl = Outcome::RateLimited {
            until: None,
            seen: false,
        };
        assert_eq!(
            waits(rl, 6),
            [60, 120, 240, 480, 600, 600].map(Some).to_vec()
        );
        assert_eq!(
            waits(Outcome::OtherStatus, 8),
            [60, 120, 240, 480, 960, 1920, 3600, 3600]
                .map(Some)
                .to_vec()
        );
        assert_eq!(
            waits(Outcome::ServerError, 7),
            vec![
                None,
                None,
                Some(60),
                Some(120),
                Some(240),
                Some(300),
                Some(300)
            ]
        );
        assert_eq!(waits(Outcome::NoCurl, 3), vec![Some(300); 3]);
        assert_eq!(waits(Outcome::NoCredentials, 2), vec![Some(300); 2]);
        assert_eq!(waits(Outcome::BadCredentials, 2), vec![Some(300); 2]);
    }

    #[test]
    fn one_streak_for_5xx_and_network() {
        let mut s = Sched::default();
        apply(&mut s, Outcome::ServerError, B);
        apply(&mut s, Outcome::Network, B);
        apply(&mut s, Outcome::ServerError, B);
        assert_eq!(s.next_attempt, Some(B + 60));
        apply(&mut s, Outcome::Success, B);
        assert_eq!(
            (s.fail_streak, s.backoff_step, s.next_attempt),
            (0, 0, None)
        );
        apply(&mut s, Outcome::ServerError, B);
        assert_eq!((s.fail_streak, s.next_attempt), (1, None));
    }

    #[test]
    fn retry_after_rules() {
        // `until` is the response time plus the header's seconds; the attempt started at B.
        let wait = |until: Option<i64>| {
            let mut s = Sched::default();
            apply(
                &mut s,
                Outcome::RateLimited {
                    until,
                    seen: until.is_some(),
                },
                B,
            );
            s.next_attempt.map(|na| na - B)
        };
        assert_eq!(wait(Some(B + 120)), Some(120));
        assert_eq!(
            wait(Some(B + 6 + 120)),
            Some(126),
            "counted from the response"
        );
        assert_eq!(
            wait(Some(B + 30)),
            Some(60),
            "smaller than the step: the step"
        );
        assert_eq!(wait(Some(B + 7200)), Some(3600), "capped at 1 h");
        assert_eq!(wait(None), Some(60));
    }

    #[test]
    fn the_two_doubling_backoffs_keep_their_own_steps() {
        let rl = Outcome::RateLimited {
            until: None,
            seen: false,
        };
        let mut s = Sched::default();
        for _ in 0..3 {
            apply(&mut s, Outcome::BadResponse, B);
        }
        apply(&mut s, rl, B);
        assert_eq!(
            s.next_attempt,
            Some(B + 60),
            "a 429 after bad responses starts at 60 s"
        );
        apply(&mut s, rl, B);
        apply(&mut s, Outcome::OtherStatus, B);
        assert_eq!(
            s.next_attempt,
            Some(B + 60),
            "a 404 after 429s starts at 60 s"
        );
        let mut s = Sched::default();
        apply(&mut s, rl, B);
        apply(&mut s, Outcome::ServerError, B);
        apply(&mut s, rl, B);
        assert_eq!(
            s.next_attempt,
            Some(B + 60),
            "a 5xx in between starts it again"
        );
        let mut s = Sched::default();
        apply(&mut s, Outcome::OtherStatus, B);
        apply(&mut s, Outcome::Network, B);
        apply(&mut s, rl, B);
        assert_eq!(
            s.next_attempt,
            Some(B + 60),
            "a 404, a network error, then a 429"
        );
        let mut s = Sched::default();
        apply(&mut s, rl, B);
        apply(&mut s, Outcome::ServerError, B);
        apply(&mut s, Outcome::OtherStatus, B);
        assert_eq!(s.next_attempt, Some(B + 60), "a 429, a 5xx, then a 404");
        let mut s = Sched::default();
        apply(&mut s, rl, B);
        apply(&mut s, Outcome::NoCurl, B);
        apply(&mut s, rl, B);
        assert_eq!(
            s.next_attempt,
            Some(B + 120),
            "a result without a call breaks no run"
        );
        let mut s = Sched::default();
        apply(&mut s, Outcome::OtherStatus, B);
        apply(&mut s, Outcome::BadResponse, B);
        assert_eq!(
            s.next_attempt,
            Some(B + 120),
            "404 and bad_response count together"
        );
    }

    #[test]
    fn a_kept_next_attempt_without_a_last_attempt() {
        let s = Sched {
            next_attempt: Some(B + 500),
            last_error: Some("rate_limited".into()),
            ..Sched::default()
        };
        assert!(!due(&s, B, true));
        assert!(due(&s, B + 500, false));
        let far = Sched {
            next_attempt: Some(B + 86_400),
            ..Sched::default()
        };
        assert!(due(&far, B, false), "over an hour ahead: ignored");
    }

    #[test]
    fn now_limits() {
        let mut s = at(B);
        assert!(!now_allowed(&s, B + 9), "10 s since the last attempt");
        assert!(now_allowed(&s, B + 10));
        s.now_times = vec![B + 12];
        assert!(!now_allowed(&s, B + 25), "one per :00/:30 window");
        assert!(now_allowed(&s, B + 31));
        s.now_times = vec![B - 500, B - 400, B - 300, B - 200, B - 100, B - 50];
        assert!(!now_allowed(&s, B + 11), "six per 10 minutes");
        s.now_times[0] = B - 700;
        assert!(now_allowed(&s, B + 11));
        let mut s = at(B);
        apply(&mut s, Outcome::Unauthorized, B);
        assert!(!now_allowed(&s, B + 40), "holds apply");
        s.now_times = vec![B - 700, B - 600, B - 599, B + 60, B + 61];
        prune_now_times(&mut s, B);
        assert_eq!(
            s.now_times,
            vec![B - 599, B + 60],
            "under 600 s old, at most 60 s ahead"
        );
    }

    #[test]
    fn words() {
        assert_eq!(Outcome::Success.word(), None);
        assert_eq!(Outcome::ServerError.word(), Some("http_error"));
        assert_eq!(Outcome::OtherStatus.word(), Some("http_error"));
        assert_eq!(Outcome::TokenExpired.word(), Some("token_expired"));
    }
}
