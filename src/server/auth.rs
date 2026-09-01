//! Access control for the HTTP surface.
//!
//! RustClaw runs every tool call without approval, so reaching this server is
//! equivalent to a shell on the machine. On loopback that is the user's own
//! terminal by another name. The moment it listens anywhere else — a Tailscale
//! address, the LAN — a bearer token becomes the only thing between a stranger
//! and `exec`, so the server refuses to bind a non-loopback address without one.

use anyhow::Context;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::IpAddr;

/// Name of the cookie the browser keeps once a token has been accepted, so a
/// phone can bookmark the page instead of re-entering the token.
pub const COOKIE: &str = "rustclaw_token";

#[derive(Clone)]
pub struct Auth {
    token: Option<String>,
}

impl Auth {
    /// `None` disables the check, which is only allowed on loopback.
    pub fn new(token: Option<String>) -> Self {
        Self { token }
    }

    pub fn required(&self) -> bool {
        self.token.is_some()
    }

    fn accepts(&self, presented: Option<&str>) -> bool {
        let Some(expected) = &self.token else { return true };
        match presented {
            Some(got) => constant_time_eq(got.as_bytes(), expected.as_bytes()),
            None => false,
        }
    }
}

/// Whether an address is reachable only from this machine.
pub fn is_loopback(bind: &str) -> bool {
    if bind.eq_ignore_ascii_case("localhost") {
        return true;
    }
    bind.parse::<IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

/// Reject the request unless it carries the token, in an `Authorization`
/// header, a `token` query parameter, or the cookie set from one.
pub async fn guard(State(auth): State<Auth>, req: Request, next: Next) -> Response {
    if !auth.required() {
        return next.run(req).await;
    }

    let headers = req.headers();
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| cookie_value(c, COOKIE));
    let query = req.uri().query().and_then(|q| query_value(q, "token"));

    let presented = bearer.or(query.clone()).or(cookie);
    if !auth.accepts(presented.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response();
    }

    let mut response = next.run(req).await;
    // A token that arrived in the URL is exchanged for a cookie, so the link
    // works once and the bookmark keeps working without the secret in the bar.
    if let Some(t) = query
        && let Ok(v) = header::HeaderValue::from_str(&format!(
            "{COOKIE}={t}; Path=/; Max-Age=31536000; SameSite=Lax; HttpOnly"
        ))
    {
        response.headers_mut().insert(header::SET_COOKIE, v);
    }
    response
}

fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.replace('+', " ");
    let bytes = bytes.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Compare without leaking where two secrets first differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A random token, hex-encoded, straight from the OS entropy source. Not worth
/// an RNG crate for twelve lines.
pub fn generate() -> anyhow::Result<String> {
    use std::io::Read;
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("reading /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_recognised_in_every_spelling() {
        for a in ["127.0.0.1", "localhost", "::1", "127.0.0.5"] {
            assert!(is_loopback(a), "{a} should be loopback");
        }
        for a in ["0.0.0.0", "100.119.106.115", "192.168.1.10", "::"] {
            assert!(!is_loopback(a), "{a} should NOT be loopback");
        }
    }

    #[test]
    fn a_configured_token_must_match_exactly() {
        let auth = Auth::new(Some("secret".into()));
        assert!(auth.accepts(Some("secret")));
        assert!(!auth.accepts(Some("secre")));
        assert!(!auth.accepts(Some("secrett")));
        assert!(!auth.accepts(Some("Secret")));
        assert!(!auth.accepts(None), "a missing token must not pass");
    }

    #[test]
    fn no_token_configured_lets_everything_through() {
        let auth = Auth::new(None);
        assert!(!auth.required());
        assert!(auth.accepts(None));
    }

    #[test]
    fn reads_the_token_out_of_a_cookie_header() {
        assert_eq!(
            cookie_value("a=1; rustclaw_token=abc; b=2", COOKIE),
            Some("abc".into())
        );
        assert_eq!(cookie_value("other=1", COOKIE), None);
    }

    #[test]
    fn reads_and_decodes_the_token_query_parameter() {
        assert_eq!(query_value("token=abc&x=1", "token"), Some("abc".into()));
        assert_eq!(query_value("x=1&token=a%2Bb", "token"), Some("a+b".into()));
        assert_eq!(query_value("x=1", "token"), None);
    }

    #[test]
    fn generated_tokens_are_long_and_distinct() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_eq!(a.len(), 48, "24 bytes hex-encoded");
        assert_ne!(a, b, "two generated tokens must not collide");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn comparison_rejects_length_mismatch_without_indexing_past_the_end() {
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"abc", b"abc"));
    }
}
