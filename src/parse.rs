//! Parsing what the endpoint sends: the usage body, ISO 8601 times, `Retry-After`.

use serde_json::{Number, Value};

/// One usage window as `get` shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// `utilization` exactly as sent (`12` stays `12`, `30.4` stays `30.4`).
    pub pct: Number,
    /// Unix seconds, or None (an idle 5-hour window).
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Windows {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

/// 2020-01-01T00:00:00Z: no `resets_at` before it is believed.
const EARLIEST: i64 = 1_577_836_800;
/// 100 years of 365.25 days.
const CENTURY: i64 = 36_525 * 86_400;

/// `pct` must be a finite number from 0 to 1000.
pub fn pct_ok(n: &Number) -> bool {
    n.as_f64()
        .is_some_and(|f| f.is_finite() && (0.0..=1000.0).contains(&f))
}

/// A 200 body: an object with `five_hour` or `seven_day` or both, each `null` or
/// `{utilization, resets_at}` (extra keys in it ignored); a missing one counts as `null`, other
/// keys are ignored. None: `bad_response`: not an object, neither key (so `{}` or an error
/// object never reads as "no windows"), or a malformed window: one bad window fails the whole
/// body.
pub fn usage(body: &[u8], now: i64) -> Option<Windows> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let m = v.as_object()?;
    let (five, seven) = (m.get("five_hour"), m.get("seven_day"));
    if five.is_none() && seven.is_none() {
        return None;
    }
    Some(Windows {
        five_hour: five.map_or(Some(None), |w| window(w, now))?,
        seven_day: seven.map_or(Some(None), |w| window(w, now))?,
    })
}

/// Outer None: not a valid window. Some(None): `null`, or `utilization: null` (no numbers;
/// `resets_at` must still be a string or `null`, and is not read).
fn window(v: &Value, now: i64) -> Option<Option<Window>> {
    let o = match v {
        Value::Null => return Some(None),
        Value::Object(o) => o,
        _ => return None,
    };
    let pct = match o.get("utilization")? {
        Value::Number(n) if pct_ok(n) => n.clone(),
        Value::Null => {
            return match o.get("resets_at")? {
                Value::Null | Value::String(_) => Some(None),
                _ => None,
            };
        }
        _ => return None,
    };
    let resets_at = match o.get("resets_at")? {
        Value::Null => None,
        Value::String(s) => {
            let t = iso8601(s)?;
            if !(EARLIEST..=now.saturating_add(CENTURY)).contains(&t) {
                return None;
            }
            Some(t)
        }
        _ => return None,
    };
    Some(Some(Window { pct, resets_at }))
}

fn digits(b: &[u8]) -> Option<i64> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number: (year, month, day).
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn seconds(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> Option<i64> {
    let ok = (1..=12).contains(&mo)
        && (1..=days_in_month(y, mo)).contains(&d)
        && (0..24).contains(&h)
        && (0..60).contains(&mi)
        && (0..60).contains(&s);
    ok.then(|| days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s)
}

/// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)` to Unix seconds, the fraction truncated.
pub fn iso8601(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let t = seconds(
        digits(&b[0..4])?,
        digits(&b[5..7])?,
        digits(&b[8..10])?,
        digits(&b[11..13])?,
        digits(&b[14..16])?,
        digits(&b[17..19])?,
    )?;
    let mut i = 19;
    if b[i] == b'.' {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    let offset = match &b[i..] {
        b"Z" | b"z" => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let (h, m) = (digits(&[*h1, *h2])?, digits(&[*m1, *m2])?);
            if h > 23 || m > 59 {
                return None;
            }
            let o = h * 3600 + m * 60;
            if *sign == b'+' { o } else { -o }
        }
        _ => return None,
    };
    Some(t - offset)
}

/// An IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) to Unix seconds.
pub fn http_date(s: &str) -> Option<i64> {
    const DAYS: [&[u8]; 7] = [b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat", b"Sun"];
    const MONTHS: [&[u8]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    let b = s.as_bytes();
    if b.len() != 29
        || !DAYS.contains(&&b[0..3])
        || &b[3..5] != b", "
        || b[7] != b' '
        || b[11] != b' '
        || b[16] != b' '
        || b[19] != b':'
        || b[22] != b':'
        || &b[25..] != b" GMT"
    {
        return None;
    }
    let mo = MONTHS.iter().position(|m| *m == &b[8..11])?;
    seconds(
        digits(&b[12..16])?,
        i64::try_from(mo).ok()? + 1,
        digits(&b[5..7])?,
        digits(&b[17..19])?,
        digits(&b[20..22])?,
        digits(&b[23..25])?,
    )
}

/// `Retry-After` as seconds from now: delay-seconds or an HTTP date. None (as absent): empty,
/// unparseable, negative, or a date in the past.
pub fn retry_after(v: &str, now: i64) -> Option<i64> {
    let v = v.trim_matches(|c| c == ' ' || c == '\t');
    if !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()) {
        // Any length: a long delay saturates (and is capped at 1 h later), never "absent".
        return Some(v.bytes().fold(0_i64, |n, d| {
            n.saturating_mul(10).saturating_add(i64::from(d - b'0'))
        }));
    }
    http_date(v).map(|t| t - now).filter(|&d| d >= 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_794_893_100; // 2026-11-17T05:25:00Z

    #[test]
    fn iso_forms() {
        assert_eq!(iso8601("2026-11-17T05:25:00Z"), Some(T));
        assert_eq!(iso8601("2026-11-17t05:25:00z"), Some(T));
        assert_eq!(iso8601("2026-11-17T05:25:00+00:00"), Some(T));
        assert_eq!(iso8601("2026-11-17T05:25:00-00:00"), Some(T));
        assert_eq!(iso8601("2026-11-17T10:55:00+05:30"), Some(T));
        assert_eq!(iso8601("2026-11-17T05:25:00.999999+00:00"), Some(T));
        assert_eq!(iso8601("2026-11-17T05:25:00.1Z"), Some(T));
        assert_eq!(iso8601("2026-11-17T00:25:00-05:00"), Some(T));
    }

    #[test]
    fn iso_refusals() {
        for s in [
            "2026-11-17T05:25:00",
            "2026-11-17T05:25Z",
            "2026-11-17 07:25:00Z",
            "2026-11-17T05:25:00.Z",
            "2026-11-17T05:25:00+0000",
            "2026-11-17T05:25:00+24:00",
            "2026-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-11-17T24:00:00Z",
            "2026-11-17T05:25:60Z",
            "+026-11-17T05:25:00Z",
            "2026-11-17T05:25:00Zjunk",
            "",
        ] {
            assert_eq!(iso8601(s), None, "{s}");
        }
        assert!(iso8601("2024-02-29T00:00:00Z").is_some());
    }

    #[test]
    fn civil_round_trip() {
        for d in [-1, 0, 1, 10_957, 20_774, 2_932_896] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(civil_from_days(T / 86_400), (2026, 11, 17));
    }

    #[test]
    fn retry_after_forms() {
        assert_eq!(retry_after("120", T), Some(120));
        assert_eq!(retry_after(" 0 ", T), Some(0));
        assert_eq!(retry_after("Tue, 17 Nov 2026 05:30:00 GMT", T), Some(300));
        assert_eq!(retry_after("Tue, 17 Nov 2026 05:20:00 GMT", T), None);
        assert_eq!(retry_after("12345678901", T), Some(12_345_678_901));
        assert_eq!(retry_after("99999999999999999999999", T), Some(i64::MAX));
        for bad in ["", "soon", "-5", "1.5", "Tue, 17 Nov 2026 07:30:00 UTC"] {
            assert_eq!(retry_after(bad, T), None, "{bad}");
        }
    }

    #[test]
    fn usage_body() {
        let w = usage(
            br#"{"five_hour":{"utilization":30.4,"resets_at":"2026-11-17T07:25:00.123+00:00"},"seven_day":{"utilization":12,"resets_at":null},"other":{"x":1}}"#,
            T,
        )
        .expect("valid");
        let f = w.five_hour.expect("five_hour");
        assert_eq!(f.pct.to_string(), "30.4");
        assert_eq!(f.resets_at, Some(T + 7200));
        let s = w.seven_day.expect("seven_day");
        assert_eq!(s.pct.to_string(), "12");
        assert_eq!(s.resets_at, None);
        let idle = usage(br#"{"five_hour":null,"seven_day":null}"#, T).expect("valid");
        assert_eq!(idle, Windows::default());
    }

    /// The shapes the endpoint's schema allows: `utilization: null`, a window key missing,
    /// extra keys in a window.
    #[test]
    fn usage_nullish() {
        let seven = r#"{"utilization":12,"resets_at":null}"#;
        // five_hour `null` in each: no numbers; seven_day's numbers shown.
        for five in [
            r#","five_hour":{"utilization":null,"resets_at":null}"#,
            r#","five_hour":{"utilization":null,"resets_at":"2026-11-17T07:25:00Z"}"#,
            r#","five_hour":{"utilization":null,"resets_at":"not a time"}"#,
            r#","other_list":[],"other_object":null"#,
        ] {
            let body = format!(r#"{{"seven_day":{seven}{five}}}"#);
            let w = usage(body.as_bytes(), T).unwrap_or_else(|| panic!("valid: {body}"));
            assert_eq!(w.five_hour, None, "{body}");
            assert_eq!(
                w.seven_day.as_ref().map(|s| s.pct.to_string()),
                Some("12".into()),
                "{body}"
            );
        }
        let only_five = usage(
            br#"{"five_hour":{"utilization":30.4,"resets_at":null,"extra":{"x":1}}}"#,
            T,
        )
        .expect("valid");
        assert_eq!(
            only_five.five_hour.map(|f| f.pct.to_string()),
            Some("30.4".into())
        );
        assert_eq!(only_five.seven_day, None);
        let both_null_util = usage(
            br#"{"five_hour":{"utilization":null,"resets_at":null},"seven_day":{"utilization":null,"resets_at":null}}"#,
            T,
        );
        assert_eq!(both_null_util, Some(Windows::default()));
        assert_eq!(usage(br#"{"five_hour":null}"#, T), Some(Windows::default()));
    }

    #[test]
    fn usage_refusals() {
        for b in [
            &br#"{"error":{"type":"rate_limit_error"}}"#[..],
            br#"{}"#,
            br#"{"other_list":[],"other_object":null,"other_null":null}"#,
            br#"{"five_hour":{"utilization":1,"resets_at":"soon"},"seven_day":null}"#,
            br#"{"five_hour":null,"seven_day":3}"#,
            br#"{"five_hour":"x"}"#,
            br#"{"seven_day":[]}"#,
            br#"{"five_hour":{"resets_at":null},"seven_day":null}"#,
            br#"{"five_hour":{"utilization":null},"seven_day":null}"#,
            br#"{"five_hour":{"utilization":null,"resets_at":5},"seven_day":null}"#,
            br#"{"five_hour":{"utilization":null,"resets_at":{}},"seven_day":null}"#,
            br#"{"five_hour":null,"seven_day":{"utilization":null,"resets_at":false}}"#,
            br#"{"five_hour":{"utilization":true,"resets_at":null},"seven_day":null}"#,
            br#"{"five_hour":{"utilization":"5","resets_at":null},"seven_day":null}"#,
            br#"{"five_hour":{"utilization":-1,"resets_at":null},"seven_day":null}"#,
            br#"{"five_hour":{"utilization":1000.5,"resets_at":null},"seven_day":null}"#,
            br#"{"five_hour":{"utilization":1},"seven_day":null}"#,
            br#"{"five_hour":{"utilization":1,"resets_at":"2019-12-31T23:59:59Z"},"seven_day":null}"#,
            br#"{"five_hour":{"utilization":1,"resets_at":"2200-01-01T00:00:00Z"},"seven_day":null}"#,
            br#"{"five_hour":{"utilization":1,"resets_at":1794900300},"seven_day":null}"#,
            b"<html>moved</html>",
            b"[]",
            b"",
        ] {
            assert_eq!(usage(b, T), None, "{}", String::from_utf8_lossy(b));
        }
    }

    #[test]
    fn usage_limits() {
        let one = |pct: &str, resets: &str| {
            let b = format!(
                r#"{{"five_hour":{{"utilization":{pct},"resets_at":{resets}}},"seven_day":null}}"#
            );
            usage(b.as_bytes(), T).is_some()
        };
        assert!(one("1000", "null"));
        assert!(one("0", "null"));
        assert!(!one("1000.001", "null"));
        assert!(one("1", r#""2020-01-01T00:00:00Z""#), "2020 exactly");
        assert!(!one("1", r#""2019-12-31T23:59:59Z""#));
        // T plus 100 years of 365.25 days: 2126-11-18T05:25:00Z.
        assert!(one("1", r#""2126-11-18T05:25:00Z""#));
        assert!(!one("1", r#""2126-11-18T05:25:01Z""#));
    }

    #[test]
    fn pct_kept_as_sent() {
        for (sent, shown) in [
            ("12", "12"),
            ("30.4", "30.4"),
            ("0", "0"),
            ("99.99", "99.99"),
        ] {
            let n: Number = serde_json::from_str(sent).expect("number");
            assert!(pct_ok(&n));
            assert_eq!(n.to_string(), shown);
        }
    }
}
