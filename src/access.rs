//! Who may use the HTTP API.
//!
//! The API can start agent processes in any directory with any environment, so it must only be
//! reachable by the user. Three checks run before every request:
//!
//! 1. **Host**: the `Host` header must name this server (localhost, the bound address, or a
//!    configured `allowed_hosts` entry). This stops DNS rebinding, where a web page makes its own
//!    domain resolve to 127.0.0.1 and then talks to the API as if it were same-origin.
//! 2. **Same origin for changes**: POST/PUT/DELETE requests sent by a page of another origin
//!    (`Origin` header, or `Sec-Fetch-Site: cross-site`) are refused. This stops cross-site request
//!    forgery, e.g. a page cancelling sessions with a form or `fetch(…, {mode: "no-cors"})`.
//! 3. **Token**: required when the server listens on a non-loopback address, or when one is
//!    configured (`auth_token` in chimera.yaml or `CHIMERA_TOKEN`). Clients send it as
//!    `Authorization: Bearer …`; a browser opens `/?token=…` once and keeps it in a cookie.

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde_json::json;

use crate::web::AppState;

/// Environment variable with the API token (overrides `auth_token` in the config file). The TUI
/// sends it too.
pub const TOKEN_ENV: &str = "CHIMERA_TOKEN";

#[derive(Debug, Clone)]
pub struct Access {
    /// Host names (lowercase, without port) the `Host` header may carry. `None`: any host, which
    /// is only used for wildcard binds (0.0.0.0), where a token is always required.
    allowed_hosts: Option<Vec<String>>,
    token: Option<String>,
    /// Name of the browser cookie that holds the token (per port, so two servers on one host do
    /// not overwrite each other's cookie).
    cookie: String,
}

/// What the server knows about its own address.
struct BindInfo {
    host: String,
    port: String,
    loopback: bool,
    wildcard: bool,
}

fn parse_bind(bind: &str) -> BindInfo {
    let (host, port) = split_host_port(bind);
    let ip = host.parse::<IpAddr>().ok();
    BindInfo {
        loopback: host == "localhost" || ip.is_some_and(|ip| ip.is_loopback()),
        wildcard: ip.is_some_and(|ip| ip.is_unspecified()),
        host,
        port: port.unwrap_or_default(),
    }
}

/// "127.0.0.1:7878" → ("127.0.0.1", "7878"); "[::1]:7878" → ("::1", "7878"); "localhost" → ("localhost", None).
fn split_host_port(s: &str) -> (String, Option<String>) {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']').unwrap_or((rest, ""));
        return (host.to_ascii_lowercase(), after.strip_prefix(':').map(str::to_owned));
    }
    match s.rsplit_once(':') {
        // A bare IPv6 address has several colons and no port.
        Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => {
            (h.to_ascii_lowercase(), Some(p.to_owned()))
        }
        _ => (s.to_ascii_lowercase(), None),
    }
}

impl Access {
    /// `configured`: token from `CHIMERA_TOKEN` or the config file. When none is configured but
    /// one is needed (non-loopback bind), it is generated once and kept in `<data_dir>/token`.
    pub fn new(
        bind: &str,
        configured: Option<String>,
        extra_hosts: &[String],
        data_dir: &Path,
    ) -> anyhow::Result<Self> {
        let info = parse_bind(bind);
        let configured = configured.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let token = match configured {
            Some(t) => Some(t),
            None if !info.loopback => Some(stored_token(data_dir)?),
            None => None,
        };
        let allowed_hosts = if info.wildcard && extra_hosts.is_empty() {
            None
        } else {
            let mut hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"].iter().map(|s| s.to_string()).collect();
            if !info.wildcard {
                hosts.push(info.host.clone());
            }
            hosts.extend(extra_hosts.iter().map(|h| split_host_port(h).0));
            hosts.sort();
            hosts.dedup();
            Some(hosts)
        };
        let cookie = format!("chimera_token_{}", if info.port.is_empty() { "default" } else { &info.port });
        Ok(Self { allowed_hosts, token, cookie })
    }

    /// Access for tests and embedded use: loopback rules, optional token.
    #[cfg(test)]
    pub fn loopback(token: Option<&str>) -> Self {
        Self {
            allowed_hosts: Some(vec!["127.0.0.1".into(), "::1".into(), "localhost".into()]),
            token: token.map(str::to_owned),
            cookie: "chimera_token_test".into(),
        }
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    fn host_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(allowed) = &self.allowed_hosts else { return true };
        let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else { return false };
        allowed.contains(&split_host_port(host).0)
    }

    fn token_from(&self, headers: &HeaderMap, query: Option<&str>) -> Option<String> {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t.trim().to_string());
        let cookie = headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|v| {
            v.split(';').find_map(|kv| {
                let (k, val) = kv.trim().split_once('=')?;
                (k == self.cookie).then(|| val.to_string())
            })
        });
        bearer.or(cookie).or_else(|| query_token(query))
    }

    fn token_ok(&self, presented: Option<&str>) -> bool {
        match (&self.token, presented) {
            (None, _) => true,
            (Some(expected), Some(given)) => constant_time_eq(expected.as_bytes(), given.as_bytes()),
            (Some(_), None) => false,
        }
    }
}

fn query_token(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|kv| kv.strip_prefix("token=")).map(str::to_owned)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Reads `<data_dir>/token`, creating it (readable by the user only) on first use.
fn stored_token(data_dir: &Path) -> anyhow::Result<String> {
    let path = data_dir.join("token");
    if let Ok(t) = std::fs::read_to_string(&path)
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

/// Whether a state-changing request comes from a page of another origin.
fn cross_site(headers: &HeaderMap) -> bool {
    if headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("cross-site") {
        return true;
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        // Not sent by a browser page (curl, the TUI): nothing cross-site about it.
        return false;
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let authority = origin.split_once("://").map(|(_, a)| a).unwrap_or(origin);
    origin == "null" || !authority.eq_ignore_ascii_case(host)
}

fn deny(status: StatusCode, msg: &str, html: bool) -> Response {
    if html {
        let page = format!(
            "<!doctype html><meta charset=utf-8><title>Chimera</title>\
             <body style=\"font-family:system-ui;max-width:40em;margin:4em auto\"><h1>Chimera</h1><p>{msg}</p>"
        );
        (status, Html(page)).into_response()
    } else {
        (status, axum::Json(json!({ "error": msg }))).into_response()
    }
}

/// Middleware applying the checks above to every request.
pub async fn guard(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let access = &st.access;
    let headers = req.headers();
    let html = !req.uri().path().starts_with("/api/");
    if !access.host_allowed(headers) {
        return deny(
            StatusCode::FORBIDDEN,
            "Unexpected Host header. Open Chimera by its own address, or add the name to allowed_hosts in chimera.yaml.",
            html,
        );
    }
    let changes = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if changes && cross_site(headers) {
        return deny(StatusCode::FORBIDDEN, "Cross-site requests are not allowed.", html);
    }
    let query = req.uri().query();
    let presented = access.token_from(headers, query);
    if !access.token_ok(presented.as_deref()) {
        return deny(
            StatusCode::UNAUTHORIZED,
            "This Chimera server needs its access token: open the URL it printed at startup \
             (it ends in ?token=…), or pass the token with --token or CHIMERA_TOKEN.",
            html,
        );
    }
    // A browser opening `/?token=…`: keep the token in a cookie and drop it from the address bar.
    if let Some(token) = query_token(query).filter(|t| access.token.is_some() && access.token_ok(Some(t)))
        && req.method() == Method::GET
    {
        let cookie = format!("{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000", access.cookie);
        let mut resp = Redirect::to(req.uri().path()).into_response();
        if let Ok(v) = cookie.parse() {
            resp.headers_mut().insert(header::SET_COOKIE, v);
        }
        return resp;
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        h
    }

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("chimera-access-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn loopback_bind_checks_host_but_needs_no_token() {
        let a = Access::new("127.0.0.1:7878", None, &[], &tmp()).unwrap();
        assert!(a.token().is_none());
        assert!(a.host_allowed(&headers(&[("host", "127.0.0.1:7878")])));
        assert!(a.host_allowed(&headers(&[("host", "localhost:7878")])));
        assert!(a.host_allowed(&headers(&[("host", "[::1]:7878")])));
        // DNS rebinding: an attacker's name resolving to 127.0.0.1.
        assert!(!a.host_allowed(&headers(&[("host", "evil.example:7878")])));
        assert!(!a.host_allowed(&headers(&[])));
    }

    #[test]
    fn non_loopback_bind_generates_and_keeps_a_token() {
        let dir = tmp();
        let a = Access::new("0.0.0.0:7878", None, &[], &dir).unwrap();
        let t = a.token().unwrap().to_string();
        assert_eq!(t.len(), 64);
        assert!(a.host_allowed(&headers(&[("host", "my-box.lan:7878")])), "wildcard bind accepts any name");
        let again = Access::new("0.0.0.0:7878", None, &[], &dir).unwrap();
        assert_eq!(again.token(), Some(t.as_str()), "the token survives restarts");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("token")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A specific address limits the Host header again.
        let lan = Access::new("192.168.1.5:7878", Some("secret".into()), &["chimera.lan".into()], &dir).unwrap();
        assert_eq!(lan.token(), Some("secret"));
        assert!(lan.host_allowed(&headers(&[("host", "192.168.1.5:7878")])));
        assert!(lan.host_allowed(&headers(&[("host", "chimera.lan")])));
        assert!(!lan.host_allowed(&headers(&[("host", "other.lan")])));
    }

    #[test]
    fn tokens_are_read_from_header_cookie_or_query() {
        let a = Access::loopback(Some("s3cret"));
        assert!(a.token_ok(a.token_from(&headers(&[("authorization", "Bearer s3cret")]), None).as_deref()));
        assert!(a.token_ok(a.token_from(&headers(&[("cookie", "x=1; chimera_token_test=s3cret")]), None).as_deref()));
        assert!(a.token_ok(a.token_from(&headers(&[]), Some("a=1&token=s3cret")).as_deref()));
        assert!(!a.token_ok(a.token_from(&headers(&[("authorization", "Bearer nope")]), None).as_deref()));
        assert!(!a.token_ok(a.token_from(&headers(&[("cookie", "chimera_token_7879=s3cret")]), None).as_deref()));
        assert!(!a.token_ok(None));
        assert!(Access::loopback(None).token_ok(None));
    }

    #[test]
    fn cross_site_changes_are_detected() {
        let same = headers(&[("host", "127.0.0.1:7878"), ("origin", "http://127.0.0.1:7878")]);
        assert!(!cross_site(&same));
        assert!(!cross_site(&headers(&[("host", "127.0.0.1:7878")])), "no Origin: not a browser page");
        assert!(cross_site(&headers(&[("host", "127.0.0.1:7878"), ("origin", "https://evil.example")])));
        assert!(cross_site(&headers(&[("host", "localhost:7878"), ("origin", "http://localhost:3000")])));
        assert!(cross_site(&headers(&[("host", "127.0.0.1:7878"), ("origin", "null")])));
        assert!(cross_site(&headers(&[("host", "127.0.0.1:7878"), ("sec-fetch-site", "cross-site")])));
    }

    #[test]
    fn bind_addresses_parse() {
        let b = parse_bind("[::1]:7878");
        assert!(b.loopback && !b.wildcard && b.host == "::1" && b.port == "7878");
        let b = parse_bind("0.0.0.0:80");
        assert!(!b.loopback && b.wildcard);
        assert!(parse_bind("localhost:7878").loopback);
        assert!(!parse_bind("10.0.0.2:7878").loopback);
    }
}
