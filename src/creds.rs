//! Claude Code's `.credentials.json`: only read, never written, refreshed or copied. One open,
//! one fstat, everything from that fd. The token is borrowed from the zeroed-on-drop read
//! buffer, never turned into a `String`; `refreshToken` and the MCP tokens are skipped as
//! `IgnoredAny`. serde's messages (they can echo values) are never printed.

use crate::paths::hex;
use crate::secret::Secret;
use crate::sys;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::Read;
use std::os::fd::AsFd;
use std::path::Path;

const MAX_BYTES: usize = 1024 * 1024;
const MAX_TOKEN: usize = 4096;
const HEADER: &[u8] = b"Authorization: Bearer ";

/// The file's identity: device, inode, size, mtime and ctime in ns.
pub fn stamp_of(st: &sys::Stat) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        st.dev, st.ino, st.size, st.mtime_ns, st.ctime_ns
    )
}

pub fn path(config_dir: &Path) -> std::path::PathBuf {
    config_dir.join(".credentials.json")
}

/// What `get` sees: a `stat` only, never an open.
pub enum Seen {
    Missing,
    Present {
        stamp: String,
        uid: libc::uid_t,
    },
    /// `stat` failed otherwise (a folder that cannot be searched).
    Unknown,
}

pub fn look(config_dir: &Path) -> Seen {
    match sys::stat(&path(config_dir)) {
        Ok(st) => Seen::Present {
            stamp: stamp_of(&st),
            uid: st.uid,
        },
        Err(e) if sys::errno_is(&e, libc::ENOENT) || sys::errno_is(&e, libc::ENOTDIR) => {
            Seen::Missing
        }
        Err(_) => Seen::Unknown,
    }
}

/// Usable credentials.
pub struct Good {
    pub stamp: String,
    /// sha256 of the salt and the raw bytes of the `claudeAiOauth` object.
    pub fingerprint: String,
    /// `expiresAt`, milliseconds.
    pub expires_ms: Option<f64>,
    /// `Authorization: Bearer <token>\n`, for curl's stdin.
    pub header: Secret,
}

impl Good {
    /// Expired by more than 60 s.
    pub fn expired(&self, now: i64) -> bool {
        self.expires_ms
            .is_some_and(|e| e < (now as f64 - 60.0) * 1000.0)
    }
}

pub enum Creds {
    Missing,
    /// No `claudeAiOauth` object (an API-key setup, or MCP logins only).
    NoOauth {
        stamp: String,
    },
    /// There, but not usable.
    Bad {
        stamp: Option<String>,
    },
    /// Not owned by the effective user: refused.
    NotOwned,
    Good(Good),
}

/// Reads the credentials once. `salt`: the state's fingerprint salt.
pub fn read(config_dir: &Path, salt: &str) -> Creds {
    let fd = match sys::open(
        &path(config_dir),
        libc::O_RDONLY | libc::O_NONBLOCK, // a FIFO cannot hang the open
    ) {
        Ok(fd) => fd,
        Err(e) if sys::errno_is(&e, libc::ENOENT) || sys::errno_is(&e, libc::ENOTDIR) => {
            return Creds::Missing;
        }
        // There but not openable (mode 000, a socket): the stamp from a stat, the same one
        // `get` sees, so that a reader does not count the file as new on every run.
        Err(_) => {
            return Creds::Bad {
                stamp: sys::stat(&path(config_dir)).ok().map(|st| stamp_of(&st)),
            };
        }
    };
    let Ok(st) = sys::fstat(fd.as_fd()) else {
        return Creds::Bad { stamp: None };
    };
    let stamp = stamp_of(&st);
    if !st.is_file() {
        return Creds::Bad { stamp: Some(stamp) };
    }
    if st.uid != sys::euid() {
        return Creds::NotOwned;
    }
    if !usize::try_from(st.size).is_ok_and(|n| n <= MAX_BYTES) {
        return Creds::Bad { stamp: Some(stamp) };
    }
    // One preallocated buffer, one byte more than allowed (a file that grew), zeroed on drop.
    let mut buf = Secret::zeroed(MAX_BYTES + 1);
    let mut f = std::fs::File::from(fd);
    let mut n = 0;
    loop {
        match f.read(&mut buf.as_mut_bytes()[n..]) {
            Ok(0) => break,
            Ok(k) => {
                n += k;
                if n > MAX_BYTES {
                    return Creds::Bad { stamp: Some(stamp) };
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Creds::Bad { stamp: Some(stamp) },
        }
    }
    match parse(&buf.as_bytes()[..n], salt) {
        Ok(Some((fingerprint, expires_ms, header))) => Creds::Good(Good {
            stamp,
            fingerprint,
            expires_ms,
            header,
        }),
        Ok(None) => Creds::NoOauth { stamp },
        Err(()) => Creds::Bad { stamp: Some(stamp) },
    }
}

/// Ok(None): no `claudeAiOauth`. Err: anything unusable.
fn parse(bytes: &[u8], salt: &str) -> Result<Option<(String, Option<f64>, Secret)>, ()> {
    let Top(raw) = serde_json::from_slice::<Top<'_>>(bytes).map_err(drop)?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let fingerprint = hex(&Sha256::new()
        .chain_update(b"claude-usage fingerprint 1\0")
        .chain_update(salt.as_bytes())
        .chain_update(b"\0")
        .chain_update(raw.get().as_bytes())
        .finalize());
    let Oauth { token, expires_ms } = serde_json::from_str::<Oauth<'_>>(raw.get()).map_err(drop)?;
    let token = token.ok_or(())?;
    let t = token.bytes();
    if !(1..=MAX_TOKEN).contains(&t.len()) || !t.iter().all(|&b| token_byte(b)) {
        return Err(());
    }
    let mut header = Secret::with_capacity(HEADER.len() + t.len() + 1);
    if !(header.push(HEADER) && header.push(t) && header.push(b"\n")) {
        return Err(());
    }
    Ok(Some((fingerprint, expires_ms, header)))
}

/// `[A-Za-z0-9._~+/=-]`: also what stops header injection through `-H @-`.
fn token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
}

/// The top level: only `claudeAiOauth`, borrowed raw; everything else skipped.
struct Top<'a>(Option<&'a RawValue>);

impl<'de> de::Deserialize<'de> for Top<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_map(TopVisitor)
    }
}

struct TopVisitor;

impl<'de> Visitor<'de> for TopVisitor {
    type Value = Top<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Top<'de>, A::Error> {
        let mut found = None;
        while let Some(Key(is)) = map.next_key_seed(KeyIs("claudeAiOauth"))? {
            if !is {
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            if found.is_some() {
                return Err(de::Error::custom("duplicate"));
            }
            let raw: &'de RawValue = map.next_value()?;
            // Only an object counts; anything else is unusable.
            if !raw.get().starts_with('{') {
                return Err(de::Error::custom("not an object"));
            }
            found = Some(raw);
        }
        Ok(Top(found))
    }
}

/// A key compared with one name, without keeping it.
struct Key(bool);
struct KeyIs(&'static str);

impl<'de> de::DeserializeSeed<'de> for KeyIs {
    type Value = Key;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Key, D::Error> {
        d.deserialize_str(self)
    }
}

impl Visitor<'_> for KeyIs {
    type Value = Key;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a key")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Key, E> {
        Ok(Key(v == self.0))
    }
}

/// The token, borrowed from the input; copied into a `Secret` only when it had JSON escapes.
enum Token<'a> {
    Borrowed(&'a [u8]),
    Owned(Secret),
}

impl Token<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Token::Borrowed(b) => b,
            Token::Owned(s) => s.as_bytes(),
        }
    }
}

struct TokenVisitor;

impl<'de> Visitor<'de> for TokenVisitor {
    type Value = Token<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string")
    }

    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Token<'de>, E> {
        Ok(Token::Borrowed(v.as_bytes()))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Token<'de>, E> {
        let mut s = Secret::with_capacity(v.len());
        if !s.push(v.as_bytes()) {
            return Err(E::custom("token"));
        }
        Ok(Token::Owned(s))
    }
}

struct TokenSeed;

impl<'de> de::DeserializeSeed<'de> for TokenSeed {
    type Value = Token<'de>;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Token<'de>, D::Error> {
        d.deserialize_str(TokenVisitor)
    }
}

/// `expiresAt`: a number (milliseconds); `null` counts as absent.
struct ExpiresVisitor;

impl Visitor<'_> for ExpiresVisitor {
    type Value = Option<f64>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a number")
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Option<f64>, E> {
        Ok(Some(v as f64))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Option<f64>, E> {
        Ok(Some(v as f64))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Option<f64>, E> {
        Ok(Some(v))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Option<f64>, E> {
        Ok(None)
    }
}

struct ExpiresSeed;

impl<'de> de::DeserializeSeed<'de> for ExpiresSeed {
    type Value = Option<f64>;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Option<f64>, D::Error> {
        d.deserialize_any(ExpiresVisitor)
    }
}

/// Inside `claudeAiOauth`: `accessToken` and `expiresAt`; everything else skipped.
struct Oauth<'a> {
    token: Option<Token<'a>>,
    expires_ms: Option<f64>,
}

impl<'de> de::Deserialize<'de> for Oauth<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_map(OauthVisitor)
    }
}

struct OauthVisitor;

impl<'de> Visitor<'de> for OauthVisitor {
    type Value = Oauth<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Oauth<'de>, A::Error> {
        let mut out = Oauth {
            token: None,
            expires_ms: None,
        };
        let mut seen_expires = false;
        while let Some(k) = map.next_key_seed(Which)? {
            match k {
                Field::Token if out.token.is_some() => {
                    return Err(de::Error::custom("duplicate"));
                }
                Field::Token => out.token = Some(map.next_value_seed(TokenSeed)?),
                Field::Expires if seen_expires => return Err(de::Error::custom("duplicate")),
                Field::Expires => {
                    seen_expires = true;
                    out.expires_ms = map.next_value_seed(ExpiresSeed)?;
                }
                Field::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(out)
    }
}

enum Field {
    Token,
    Expires,
    Other,
}

struct Which;

impl<'de> de::DeserializeSeed<'de> for Which {
    type Value = Field;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Field, D::Error> {
        d.deserialize_str(self)
    }
}

impl Visitor<'_> for Which {
    type Value = Field;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a key")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Field, E> {
        Ok(match v {
            "accessToken" => Field::Token,
            "expiresAt" => Field::Expires,
            _ => Field::Other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SALT: &str = "0123456789abcdef0123456789abcdef";

    fn header(b: &[u8]) -> Result<Option<Vec<u8>>, ()> {
        parse(b, SALT).map(|o| o.map(|(_, _, h)| h.as_bytes().to_vec()))
    }

    #[test]
    fn the_token_line() {
        let b = br#"{"claudeAiOauth":{"accessToken":"TEST-TOKEN-AB_c.d~e+f/g=-","refreshToken":"R","expiresAt":1794893100000,"scopes":["a"]},"mcpOAuth":{"s":{"accessToken":"M"}}}"#;
        // Built from parts: the privacy check refuses a long bearer token written out.
        let want = [HEADER, b"TEST-TOKEN-AB_c.d~e+f/g=-", b"\n"].concat();
        assert_eq!(header(b), Ok(Some(want)));
        let (_, exp, _) = parse(b, SALT).expect("ok").expect("some");
        assert_eq!(exp, Some(1_794_893_100_000.0));
    }

    #[test]
    fn escapes_are_decoded_then_checked() {
        assert_eq!(
            header(br#"{"claudeAiOauth":{"accessToken":"a\/b"}}"#),
            Ok(Some(b"Authorization: Bearer a/b\n".to_vec()))
        );
        assert_eq!(
            header(br#"{"claudeAiOauth":{"accessToken":"A\r\nX-Injected: 1"}}"#),
            Err(())
        );
    }

    #[test]
    fn refusals() {
        let long = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{}"}}}}"#,
            "A".repeat(4097)
        );
        for b in [
            &br#"{"claudeAiOauth":{"expiresAt":1}}"#[..],
            br#"{"claudeAiOauth":{"accessToken":""}}"#,
            br#"{"claudeAiOauth":{"accessToken":"a b"}}"#,
            br#"{"claudeAiOauth":{"accessToken":5}}"#,
            br#"{"claudeAiOauth":{"accessToken":"A","expiresAt":"soon"}}"#,
            br#"{"claudeAiOauth":{"accessToken":"A","accessToken":"B"}}"#,
            br#"{"claudeAiOauth":{"accessToken":"A"},"claudeAiOauth":{"accessToken":"B"}}"#,
            br#"{"claudeAiOauth":"A"}"#,
            br#"{"claudeAiOauth":null}"#,
            br#"{"claudeAiOauth":"#,
            b"[]",
            b"",
            long.as_bytes(),
        ] {
            assert_eq!(header(b), Err(()), "{}", String::from_utf8_lossy(b));
        }
        assert!(
            header(
                format!(
                    r#"{{"claudeAiOauth":{{"accessToken":"{}"}}}}"#,
                    "A".repeat(4096)
                )
                .as_bytes()
            )
            .is_ok()
        );
    }

    #[test]
    fn expires_at_forms() {
        let exp = |v: &str| {
            let b = format!(r#"{{"claudeAiOauth":{{"accessToken":"A","expiresAt":{v}}}}}"#);
            parse(b.as_bytes(), SALT).map(|o| o.map(|(_, e, _)| e))
        };
        assert_eq!(exp("1794893100000"), Ok(Some(Some(1_794_893_100_000.0))));
        assert_eq!(exp("1.7948931e12"), Ok(Some(Some(1_794_893_100_000.0))));
        assert_eq!(exp("-5"), Ok(Some(Some(-5.0))));
        assert_eq!(exp("null"), Ok(Some(None)), "null counts as absent");
        assert_eq!(exp("true"), Err(()));
        assert_eq!(exp("\"1\""), Err(()));
        assert_eq!(exp("{}"), Err(()));
        assert_eq!(
            header(br#"{"claudeAiOauth":{"accessToken":"A","expiresAt":1,"expiresAt":2}}"#),
            Err(()),
            "duplicate expiresAt"
        );
    }

    #[test]
    fn only_the_top_level_counts() {
        assert_eq!(
            header(br#"{"x":{"claudeAiOauth":{"accessToken":"A"}}}"#),
            Ok(None)
        );
        assert_eq!(
            header(br#"{"claudeAiOauth":{"x":{"accessToken":"A"}}}"#),
            Err(()),
            "a nested accessToken is not the token"
        );
    }

    #[test]
    fn no_claude_login() {
        assert_eq!(
            header(br#"{"mcpOAuth":{"s":{"accessToken":"M"}}}"#),
            Ok(None)
        );
        assert_eq!(header(b"{}"), Ok(None));
    }

    #[test]
    fn fingerprint_covers_only_claude_ai_oauth() {
        let fp = |b: &[u8], salt: &str| parse(b, salt).expect("ok").expect("some").0;
        let a = fp(
            br#"{"claudeAiOauth":{"accessToken":"A"},"mcpOAuth":{"x":1}}"#,
            SALT,
        );
        let mcp = fp(
            br#"{"mcpOAuth":{"x":2},"claudeAiOauth":{"accessToken":"A"}}"#,
            SALT,
        );
        let other = fp(br#"{"claudeAiOauth":{"accessToken":"B"}}"#, SALT);
        let salted = fp(br#"{"claudeAiOauth":{"accessToken":"A"}}"#, &"f".repeat(32));
        assert_eq!(a, mcp);
        assert_ne!(a, other);
        assert_ne!(a, salted);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn expiry_has_60_s_of_grace() {
        let g = |ms: Option<f64>| Good {
            stamp: String::new(),
            fingerprint: String::new(),
            expires_ms: ms,
            header: Secret::with_capacity(0),
        };
        let now = 1_794_893_100;
        assert!(!g(None).expired(now));
        assert!(!g(Some((now as f64 - 30.0) * 1000.0)).expired(now));
        assert!(
            !g(Some((now as f64 - 60.0) * 1000.0)).expired(now),
            "exactly 60 s"
        );
        assert!(g(Some((now as f64 - 61.0) * 1000.0)).expired(now));
    }
}
