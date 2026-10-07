//! Request ids, client addresses, and the one log line each request gets.
//!
//! The middleware gives every request a [`RequestContext`] for as long as it
//! is handled. Log lines written meanwhile end with ` rid=<id>` (see
//! [`crate::config::setup_logger`]), pages carry the id in
//! `<meta name="request-id">`, and the response echoes it in `X-Request-Id`.
//! Once the response is ready one line is written with target `http`:
//!
//! ```text
//! http rid=<id> prid=<id|-> sid=<sid|-> ip=<ip> method=<M> route=<route> status=<code> ms=<int> user=<16 hex|->
//! ```
//!
//! The route is the matched template, or the path when nothing matched;
//! query strings, bodies and headers are never logged. Probes, metrics and
//! static assets get no line.
//!
//! The proxy in front of the oracle sets `X-Request-Id` and the client's
//! address (`X-Real-IP` by default). Both are believed only when the TCP
//! peer is one of the trusted proxies; otherwise the oracle makes its own
//! id and logs the peer's address.

use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{Arc, OnceLock},
    time::Instant,
};

use axum::{
    extract::{ConnectInfo, MatchedPath, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use log::info;
use nostr::key::PublicKey;

pub const REQUEST_ID: &str = "x-request-id";
pub const PARENT_REQUEST_ID: &str = "x-parent-request-id";
pub const SESSION_ID: &str = "x-session-id";
/// Where the proxy puts the client's address unless configured otherwise.
pub const DEFAULT_CLIENT_IP_HEADER: &str = "X-Real-IP";

/// Longest value a log line carries, in characters.
const MAX_VALUE_CHARS: usize = 200;

tokio::task_local! {
    static CURRENT: RequestContext;
}

/// Who a request came from and which request it is.
#[derive(Clone, Debug)]
pub struct RequestContext {
    /// This request's id: the proxy's, or one made here.
    pub rid: String,
    /// The calling service's request id, from `X-Parent-Request-Id`.
    pub prid: Option<String>,
    /// The browser tab's session id, from `X-Session-Id`.
    pub sid: Option<String>,
    /// The client's address; `None` only without a TCP peer (tests).
    pub ip: Option<IpAddr>,
    /// Whether pages load the browser telemetry beacon.
    pub telemetry: bool,
    /// The first 16 hex characters of the NIP-98 signer, once verified.
    user: Arc<OnceLock<String>>,
}

impl RequestContext {
    /// The client address as logged: `-` when unknown.
    pub fn ip_label(&self) -> String {
        self.ip.map_or_else(|| "-".to_owned(), |ip| ip.to_string())
    }
}

/// Runs `f` on the context of the request being handled, if any.
pub fn current<T>(f: impl FnOnce(&RequestContext) -> T) -> Option<T> {
    CURRENT.try_with(f).ok()
}

/// Runs `future` as if it handled a request with `context`.
#[cfg(test)]
pub(crate) async fn scope<F: Future>(context: RequestContext, future: F) -> F::Output {
    CURRENT.scope(context, future).await
}

/// The id of the request being handled, if any.
pub fn current_rid() -> Option<String> {
    current(|context| context.rid.clone())
}

/// Records the verified signer of the request being handled for its log
/// line.
pub fn set_user(pubkey: &PublicKey) {
    current(|context| {
        let hex = pubkey.to_hex();
        let _ = context.user.set(hex[..16].to_owned());
    });
}

/// What lines logged while a request is handled end with: ` rid=<id>`, or
/// nothing outside a request. The request and browser event lines carry
/// their ids already.
pub fn log_suffix(target: &str) -> String {
    if target == "http" || target == crate::telemetry::LOG_TARGET {
        return String::new();
    }
    current(|context| format!(" rid={}", context.rid)).unwrap_or_default()
}

/// A network, `address/prefix`, or one address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 peer on a dual-stack socket arrives as ::ffff:a.b.c.d.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        match (self.network, ip) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(network) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(network) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// A trusted proxy that is neither an address nor `address/prefix`.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidCidr;

impl FromStr for Cidr {
    type Err = InvalidCidr;

    fn from_str(value: &str) -> Result<Self, InvalidCidr> {
        let (address, prefix) = match value.trim().split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (value.trim(), None),
        };
        let network: IpAddr = address.parse().map_err(|_| InvalidCidr)?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.parse::<u8>().map_err(|_| InvalidCidr)?,
            None => max,
        };
        if prefix > max {
            return Err(InvalidCidr);
        }
        Ok(Self { network, prefix })
    }
}

/// Validated `[http_context]` and `[telemetry]` settings.
#[derive(Clone, Debug)]
pub struct RequestSettings {
    pub trusted_proxies: Vec<Cidr>,
    pub client_ip_header: HeaderName,
    pub telemetry: bool,
}

impl Default for RequestSettings {
    fn default() -> Self {
        Self {
            trusted_proxies: Vec::new(),
            client_ip_header: HeaderName::from_static("x-real-ip"),
            telemetry: false,
        }
    }
}

impl RequestSettings {
    fn trusts(&self, peer: Option<IpAddr>) -> bool {
        peer.is_some_and(|peer| self.trusted_proxies.iter().any(|cidr| cidr.contains(peer)))
    }

    /// The context for a request from `peer` with `headers`.
    pub fn context(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> RequestContext {
        let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        let trusted = self.trusts(peer);
        let rid = header(REQUEST_ID)
            .filter(|rid| trusted && is_request_id(rid))
            .map_or_else(|| uuid::Uuid::now_v7().to_string(), str::to_owned);
        let ip = header(self.client_ip_header.as_str())
            .filter(|_| trusted)
            .and_then(|ip| ip.trim().parse().ok())
            .or(peer);
        RequestContext {
            rid,
            prid: header(PARENT_REQUEST_ID)
                .filter(|prid| is_request_id(prid))
                .map(str::to_owned),
            sid: header(SESSION_ID)
                .filter(|sid| is_session_id(sid))
                .map(str::to_owned),
            ip,
            telemetry: self.telemetry,
            user: Arc::default(),
        }
    }
}

/// `^[0-9A-Za-z-]{8,64}$`
pub fn is_request_id(value: &str) -> bool {
    (8..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// `^[A-Za-z0-9_-]{16,32}$`
pub fn is_session_id(value: &str) -> bool {
    (16..=32).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// A value as a log line carries it: control characters removed, at most
/// [`MAX_VALUE_CHARS`] characters, and in double quotes (with `"` and `\`
/// escaped) when it has a space, `"` or `=`. Empty values are `""`.
pub fn log_value(value: &str) -> String {
    let clean: String = value
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_VALUE_CHARS)
        .collect();
    if !clean.is_empty() && !clean.contains([' ', '"', '=']) {
        return clean;
    }
    let mut quoted = String::with_capacity(clean.len() + 2);
    quoted.push('"');
    for c in clean.chars() {
        if c == '"' || c == '\\' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// Probes, metrics and static assets get no request line.
fn is_quiet(path: &str) -> bool {
    path == "/metrics"
        || path.starts_with("/health")
        || path == "/ready"
        || ["/assets/", "/ui/pkg/", "/static/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

/// Middleware: handles the request inside its [`RequestContext`], echoes the
/// id in `X-Request-Id`, and writes the request line.
pub async fn request_context(
    State(settings): State<Arc<RequestSettings>>,
    request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip());
    let context = settings.context(peer, request.headers());
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| path.clone(), |matched| matched.as_str().to_owned());
    let user = context.user.clone();
    let line_context = context.clone();
    let mut response = CURRENT.scope(context, next.run(request)).await;
    if let Ok(value) = HeaderValue::from_str(&line_context.rid) {
        response.headers_mut().insert(REQUEST_ID, value);
    }
    if !is_quiet(&path) {
        info!(
            target: "http",
            "http rid={} prid={} sid={} ip={} method={} route={} status={} ms={} user={}",
            line_context.rid,
            line_context.prid.as_deref().unwrap_or("-"),
            line_context.sid.as_deref().unwrap_or("-"),
            line_context.ip_label(),
            log_value(method.as_str()),
            log_value(&route),
            response.status().as_u16(),
            started.elapsed().as_millis(),
            user.get().map_or("-", String::as_str),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(proxies: &[&str]) -> RequestSettings {
        RequestSettings {
            trusted_proxies: proxies.iter().map(|cidr| cidr.parse().unwrap()).collect(),
            ..RequestSettings::default()
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn cidrs_match_their_networks() {
        let lan: Cidr = "10.77.0.0/16".parse().unwrap();
        assert!(lan.contains("10.77.3.4".parse().unwrap()));
        assert!(!lan.contains("10.78.0.1".parse().unwrap()));
        assert!(lan.contains("::ffff:10.77.0.1".parse().unwrap()));
        let one: Cidr = "127.0.0.1".parse().unwrap();
        assert!(one.contains("127.0.0.1".parse().unwrap()));
        assert!(!one.contains("127.0.0.2".parse().unwrap()));
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains("203.0.113.9".parse().unwrap()));
        let v6: Cidr = "fd00::/8".parse().unwrap();
        assert!(v6.contains("fd12::1".parse().unwrap()));
        assert!(!v6.contains("10.0.0.1".parse().unwrap()));
        for invalid in ["10.0.0.0/33", "::/129", "nope", "10.0.0.0/x", ""] {
            assert!(invalid.parse::<Cidr>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn a_trusted_proxy_supplies_the_id_and_the_client_address() {
        let proxy = Some("127.0.0.1".parse().unwrap());
        let sent = headers(&[
            ("x-request-id", "0192f3a0-0000-7000-8000-0000000000aa"),
            ("x-real-ip", "203.0.113.9"),
        ]);
        let context = settings(&["127.0.0.1/32"]).context(proxy, &sent);
        assert_eq!(context.rid, "0192f3a0-0000-7000-8000-0000000000aa");
        assert_eq!(context.ip, Some("203.0.113.9".parse().unwrap()));

        let untrusted = settings(&[]).context(proxy, &sent);
        assert_ne!(untrusted.rid, "0192f3a0-0000-7000-8000-0000000000aa");
        assert!(is_request_id(&untrusted.rid));
        assert_eq!(untrusted.ip, proxy, "the peer, not the header");
    }

    #[test]
    fn invalid_ids_are_replaced_or_dropped() {
        let proxy = Some("127.0.0.1".parse().unwrap());
        let sent = headers(&[
            ("x-request-id", "short"),
            ("x-parent-request-id", "has spaces in it"),
            ("x-session-id", "abc"),
            ("x-real-ip", "not an address"),
        ]);
        let context = settings(&["127.0.0.1"]).context(proxy, &sent);
        assert_ne!(context.rid, "short");
        assert_eq!(context.prid, None);
        assert_eq!(context.sid, None);
        assert_eq!(context.ip, proxy);

        let sent = headers(&[
            ("x-parent-request-id", "0192f3a0-0000-7000"),
            ("x-session-id", "AbCdEfGhIjKlMnOpQrStUv"),
        ]);
        let context = settings(&[]).context(None, &sent);
        assert_eq!(context.prid.as_deref(), Some("0192f3a0-0000-7000"));
        assert_eq!(context.sid.as_deref(), Some("AbCdEfGhIjKlMnOpQrStUv"));
        assert_eq!(context.ip_label(), "-");
    }

    #[test]
    fn values_are_cleaned_and_quoted() {
        assert_eq!(
            log_value("/oracle/events/{event_id}"),
            "/oracle/events/{event_id}"
        );
        assert_eq!(log_value("a b"), "\"a b\"");
        assert_eq!(log_value("k=v"), "\"k=v\"");
        assert_eq!(log_value("say \"hi\" \\"), "\"say \\\"hi\\\" \\\\\"");
        assert_eq!(log_value("line\nbreak\u{7}"), "linebreak");
        assert_eq!(log_value(""), "\"\"");
        assert_eq!(log_value(&"x".repeat(500)).len(), 200);
    }

    #[test]
    fn probes_metrics_and_assets_are_quiet() {
        for path in [
            "/metrics",
            "/health",
            "/healthy",
            "/ready",
            "/assets/site.0.js",
        ] {
            assert!(is_quiet(path), "{path}");
        }
        for path in ["/", "/events", "/oracle/events", "/api/v1/telemetry"] {
            assert!(!is_quiet(path), "{path}");
        }
    }

    #[tokio::test]
    async fn lines_logged_inside_a_request_carry_its_id() {
        assert_eq!(log_suffix("oracle"), "");
        let context = settings(&[]).context(None, &HeaderMap::new());
        let rid = context.rid.clone();
        CURRENT
            .scope(context, async {
                assert_eq!(log_suffix("oracle::routes"), format!(" rid={rid}"));
                assert_eq!(log_suffix("http"), "");
                assert_eq!(log_suffix("ui_event"), "");
                let keys = nostr::key::Keys::generate();
                set_user(&keys.public_key());
                let user = current(|context| context.user.get().cloned()).flatten();
                assert_eq!(user, Some(keys.public_key().to_hex()[..16].to_owned()));
            })
            .await;
    }
}
