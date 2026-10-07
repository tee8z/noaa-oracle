//! Browser telemetry: `POST /api/v1/telemetry`, off unless
//! `telemetry.enabled`.
//!
//! The beacon (`templates/layouts/telemetry.js`) sends batches of page
//! views, vitals, clicks, htmx requests and script errors. Each accepted
//! event becomes one line with target `ui_event`:
//!
//! ```text
//! ui_event site=4casttruth rid=<page rid|-> sid=<sid> ip=<ip> ev=<name> page=<path> t=<ms> k=v …
//! ```
//!
//! Only known events and fields are kept, each with its expected type;
//! strings lose their query, control characters and anything past 200
//! characters, and those that look like keys, invoices or email addresses
//! become `[redacted]`. A session may send [`SESSION_EVENTS_PER_HOUR`]
//! events an hour and the oracle accepts [`GLOBAL_EVENTS_PER_SECOND`] a
//! second; events past either are dropped and counted in
//! `oracle_telemetry_events_dropped_total`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
};
use log::info;
use serde_json::{Map, Value};

use crate::{
    AppState,
    request_context::{self, is_request_id, is_session_id, log_value},
};

pub const LOG_TARGET: &str = "ui_event";
/// The `site` every line carries.
const SITE: &str = "4casttruth";
/// Largest body accepted; larger ones get 413.
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// Events read from one batch; the rest are dropped.
const MAX_BATCH_EVENTS: usize = 50;
const MAX_STRING_CHARS: usize = 200;
const MAX_CLICK_TEXT_CHARS: usize = 40;
pub const SESSION_EVENTS_PER_HOUR: u32 = 500;
pub const GLOBAL_EVENTS_PER_SECOND: u32 = 50;
/// Sessions counted at once. Past this, events from new sessions are
/// dropped until an hour-long window ends.
const MAX_SESSIONS: usize = 10_000;
const SESSION_WINDOW: Duration = Duration::from_secs(60 * 60);
const REDACTED: &str = "[redacted]";

#[derive(Clone, Copy)]
enum Kind {
    Integer,
    Number,
    Text,
    /// A path or file name: its query and fragment are cut off.
    Path,
}

/// Each event type and the fields it may carry, in the order lines print
/// them.
const EVENTS: &[(&str, &[(&str, Kind)])] = &[
    (
        "page_view",
        &[
            ("ref", Kind::Path),
            ("ttfb", Kind::Integer),
            ("dcl", Kind::Integer),
            ("load", Kind::Integer),
        ],
    ),
    (
        "vitals",
        &[
            ("lcp", Kind::Integer),
            ("cls", Kind::Number),
            ("inp", Kind::Integer),
        ],
    ),
    (
        "click",
        &[
            ("el", Kind::Text),
            ("id", Kind::Text),
            ("track", Kind::Text),
            ("text", Kind::Text),
        ],
    ),
    ("submit", &[("form", Kind::Text)]),
    (
        "htmx",
        &[
            ("verb", Kind::Text),
            ("path", Kind::Path),
            ("status", Kind::Integer),
            ("ms", Kind::Integer),
            ("rid", Kind::Text),
        ],
    ),
    (
        "js_error",
        &[
            ("msg", Kind::Text),
            ("src", Kind::Path),
            ("line", Kind::Integer),
        ],
    ),
    ("mark", &[("name", Kind::Text)]),
];

/// A batch as sent, after validation.
#[derive(Debug, PartialEq)]
pub struct Batch {
    pub sid: String,
    /// The page's request id, when it is a valid one.
    pub rid: Option<String>,
    pub events: Vec<UiEvent>,
    /// Events beyond [`MAX_BATCH_EVENTS`] or of unknown types.
    pub skipped: usize,
}

/// One event, with its fields already cleaned and formatted.
#[derive(Debug, PartialEq)]
pub struct UiEvent {
    pub ev: &'static str,
    pub page: String,
    pub t: Option<String>,
    pub fields: Vec<(&'static str, String)>,
}

#[derive(Debug, PartialEq)]
pub struct Malformed;

/// Reads a batch: an object with a valid `sid`, an optional `rid` and an
/// `events` array. Anything else is [`Malformed`].
pub fn parse_batch(body: &[u8]) -> Result<Batch, Malformed> {
    let value: Value = serde_json::from_slice(body).map_err(|_| Malformed)?;
    let object = value.as_object().ok_or(Malformed)?;
    let sid = object
        .get("sid")
        .and_then(Value::as_str)
        .filter(|sid| is_session_id(sid))
        .ok_or(Malformed)?
        .to_owned();
    let rid = match object.get("rid") {
        None | Some(Value::Null) => None,
        Some(Value::String(rid)) => Some(rid.clone()).filter(|rid| is_request_id(rid)),
        Some(_) => return Err(Malformed),
    };
    let sent = object
        .get("events")
        .and_then(Value::as_array)
        .ok_or(Malformed)?;
    let events: Vec<UiEvent> = sent
        .iter()
        .take(MAX_BATCH_EVENTS)
        .filter_map(|event| event.as_object().and_then(parse_event))
        .collect();
    Ok(Batch {
        sid,
        rid,
        skipped: sent.len() - events.len(),
        events,
    })
}

fn parse_event(event: &Map<String, Value>) -> Option<UiEvent> {
    let sent = event.get("ev")?.as_str()?;
    let &(ev, allowed) = EVENTS.iter().find(|(ev, _)| *ev == sent)?;
    let page = event
        .get("page")
        .and_then(|page| clean(page, Kind::Path))
        .unwrap_or_else(|| "-".to_owned());
    let t = event.get("t").and_then(|t| clean(t, Kind::Integer));
    let fields = allowed
        .iter()
        .filter_map(|(name, kind)| {
            let mut value = clean(event.get(*name)?, *kind)?;
            if ev == "click" && *name == "text" {
                value = value.chars().take(MAX_CLICK_TEXT_CHARS).collect();
            }
            Some((*name, value))
        })
        .collect();
    Some(UiEvent {
        ev,
        page,
        t,
        fields,
    })
}

/// A field's value if it has the expected type, cleaned and scrubbed.
fn clean(value: &Value, kind: Kind) -> Option<String> {
    match kind {
        Kind::Integer => {
            let number = value.as_f64()?;
            (number.is_finite() && number.abs() < 1e15)
                .then(|| format!("{}", number.round() as i64))
        }
        Kind::Number => {
            let number = value.as_f64()?;
            if !number.is_finite() || number.abs() >= 1e15 {
                return None;
            }
            let text = format!("{number:.4}");
            Some(text.trim_end_matches('0').trim_end_matches('.').to_owned())
        }
        Kind::Text => Some(scrub_text(value.as_str()?)),
        Kind::Path => {
            let text = value.as_str()?;
            let end = text.find(['?', '#']).unwrap_or(text.len());
            Some(scrub_text(&text[..end]))
        }
    }
}

/// Control characters removed, scrubbed, then at most 200 characters.
fn scrub_text(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    scrub(&clean).chars().take(MAX_STRING_CHARS).collect()
}

/// `[redacted]` for anything that looks like a Nostr or extended key, a
/// Lightning invoice or LNURL, 40 or more hex digits, or an email address.
pub fn scrub(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let secret = [
        "nsec1", "npub1", "lnbc", "lntb", "lntbs", "lnurl", "xprv", "tprv",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if secret || has_long_hex(text) || has_email(text) {
        REDACTED.to_owned()
    } else {
        text.to_owned()
    }
}

fn has_long_hex(text: &str) -> bool {
    let mut run = 0;
    for byte in text.bytes() {
        run = if byte.is_ascii_hexdigit() { run + 1 } else { 0 };
        if run >= 40 {
            return true;
        }
    }
    false
}

/// `local@domain.tld`, roughly: a local part character before an `@`, and
/// after it a label, a dot and two letters.
fn has_email(text: &str) -> bool {
    let bytes = text.as_bytes();
    let local = |byte: u8| byte.is_ascii_alphanumeric() || b"._%+-".contains(&byte);
    let label = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-';
    bytes.iter().enumerate().any(|(at, &byte)| {
        if byte != b'@' || at == 0 || !local(bytes[at - 1]) {
            return false;
        }
        let domain = &bytes[at + 1..];
        let domain_end = domain
            .iter()
            .position(|&byte| !(label(byte) || byte == b'.'))
            .unwrap_or(domain.len());
        let domain = &domain[..domain_end];
        domain
            .iter()
            .position(|&byte| byte == b'.')
            .is_some_and(|dot| {
                dot > 0
                    && domain[dot + 1..]
                        .iter()
                        .take_while(|byte| byte.is_ascii_alphabetic())
                        .count()
                        >= 2
            })
    })
}

/// The `ui_event` line for one event.
pub fn event_line(rid: Option<&str>, sid: &str, ip: &str, event: &UiEvent) -> String {
    let mut line = format!(
        "ui_event site={SITE} rid={} sid={sid} ip={ip} ev={} page={}",
        rid.unwrap_or("-"),
        event.ev,
        log_value(&event.page)
    );
    if let Some(t) = &event.t {
        line.push_str(&format!(" t={t}"));
    }
    for (name, value) in &event.fields {
        // An htmx event's own `rid` (its reply's id) is written as `hx_rid`, so `rid=` on
        // the line stays the page's id, as on the coordinator's lines.
        let name = if *name == "rid" { "hx_rid" } else { name };
        line.push_str(&format!(" {name}={}", log_value(value)));
    }
    line
}

/// The per-session and global caps.
pub struct Limits {
    state: Mutex<LimitState>,
}

struct LimitState {
    /// Events counted per session since its window started.
    sessions: HashMap<String, (Instant, u32)>,
    tokens: f64,
    refilled: Instant,
}

impl Default for Limits {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl Limits {
    pub fn new(now: Instant) -> Self {
        Self {
            state: Mutex::new(LimitState {
                sessions: HashMap::new(),
                tokens: f64::from(GLOBAL_EVENTS_PER_SECOND),
                refilled: now,
            }),
        }
    }

    /// How many of `wanted` events from `sid` may be logged at `now`.
    pub fn admit(&self, sid: &str, wanted: usize, now: Instant) -> usize {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let state = &mut *guard;
        let rate = f64::from(GLOBAL_EVENTS_PER_SECOND);
        let elapsed = now.saturating_duration_since(state.refilled).as_secs_f64();
        state.tokens = (state.tokens + elapsed * rate).min(rate);
        state.refilled = now;

        if !state.sessions.contains_key(sid) && state.sessions.len() >= MAX_SESSIONS {
            state
                .sessions
                .retain(|_, (started, _)| now.saturating_duration_since(*started) < SESSION_WINDOW);
            if state.sessions.len() >= MAX_SESSIONS {
                return 0;
            }
        }
        let session = state.sessions.entry(sid.to_owned()).or_insert((now, 0));
        if now.saturating_duration_since(session.0) >= SESSION_WINDOW {
            *session = (now, 0);
        }
        let session_left = SESSION_EVENTS_PER_HOUR.saturating_sub(session.1) as usize;
        let admitted = wanted.min(session_left).min(state.tokens.floor() as usize);
        session.1 += admitted as u32;
        state.tokens -= admitted as f64;
        admitted
    }
}

/// `POST /api/v1/telemetry`: 204 with or without dropped events, 400 for a
/// body that isn't a batch, 413 above [`MAX_BODY_BYTES`], 404 when off.
pub async fn telemetry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if !state.request_settings().telemetry {
        return StatusCode::NOT_FOUND;
    }
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase());
    if !matches!(
        content_type.as_deref(),
        Some("application/json" | "text/plain")
    ) {
        return StatusCode::BAD_REQUEST;
    }
    let Ok(batch) = parse_batch(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    let admitted = state
        .telemetry_limits()
        .admit(&batch.sid, batch.events.len(), Instant::now());
    let dropped = batch.skipped + batch.events.len() - admitted;
    if dropped > 0 {
        state.metrics().telemetry_events_dropped(dropped);
    }
    let ip = request_context::current(|context| context.ip_label()).unwrap_or_else(|| "-".into());
    for event in batch.events.iter().take(admitted) {
        info!(
            target: LOG_TARGET,
            "{}",
            event_line(batch.rid.as_deref(), &batch.sid, &ip, event)
        );
    }
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SID: &str = "AbCdEfGhIjKlMnOpQrStUv";

    fn batch(events: Value) -> Batch {
        parse_batch(
            json!({"sid": SID, "rid": null, "events": events})
                .to_string()
                .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn keys_invoices_hex_and_emails_are_redacted() {
        for secret in [
            "nsec1qqqq",
            "my NPUB1abc",
            "lnbc10u1p3xyz",
            "LNTB1",
            "lntbs500",
            "LNURL1DP68",
            "xprv9s21",
            "tprv8Z",
            &"ab".repeat(20),
            "write to someone@example.org please",
            "a.b+c@mail.co",
        ] {
            assert_eq!(scrub(secret), REDACTED, "{secret}");
        }
        for fine in [
            "Forecasts",
            "/events/0192f3a0-0000-7000-8000-0000000000aa",
            &"ab".repeat(19),
            "@handle",
            "user@localhost",
            "a@b.c",
        ] {
            assert_eq!(scrub(fine), fine, "{fine}");
        }
    }

    #[test]
    fn only_known_events_and_typed_fields_are_kept() {
        let parsed = batch(json!([
            {"ev": "click", "t": 12.6, "page": "/events?x=1", "el": "button", "id": 7,
             "text": "x".repeat(60), "value": "typed by the reader"},
            {"ev": "keypress", "t": 1, "page": "/"},
            "not an object",
            {"ev": "vitals", "page": "/", "lcp": 812.4, "cls": 0.01234, "inp": "slow"},
            {"ev": "js_error", "page": "/", "msg": "boom\nagain", "src": "/assets/site.1.js?v=2#x", "line": 3},
        ]));
        assert_eq!(parsed.skipped, 2);
        assert_eq!(parsed.events.len(), 3);
        let click = &parsed.events[0];
        assert_eq!(click.ev, "click");
        assert_eq!(click.page, "/events");
        assert_eq!(click.t.as_deref(), Some("13"));
        assert_eq!(
            click.fields,
            vec![("el", "button".to_owned()), ("text", "x".repeat(40))],
            "id has the wrong type, value is unknown"
        );
        assert_eq!(
            parsed.events[1].fields,
            vec![("lcp", "812".to_owned()), ("cls", "0.0123".to_owned())]
        );
        assert_eq!(
            parsed.events[2].fields,
            vec![
                ("msg", "boomagain".to_owned()),
                ("src", "/assets/site.1.js".to_owned()),
                ("line", "3".to_owned())
            ]
        );
    }

    #[test]
    fn strings_are_truncated() {
        let parsed = batch(json!([{"ev": "mark", "page": "/", "name": "y".repeat(500)}]));
        assert_eq!(parsed.events[0].fields[0].1.chars().count(), 200);
    }

    #[test]
    fn batches_keep_at_most_fifty_events() {
        let events: Vec<Value> = (0..60)
            .map(|_| json!({"ev": "mark", "name": "a"}))
            .collect();
        let parsed = batch(Value::Array(events));
        assert_eq!(parsed.events.len(), 50);
        assert_eq!(parsed.skipped, 10);
    }

    #[test]
    fn malformed_batches_are_rejected() {
        for body in [
            "not json",
            "[]",
            r#"{"rid": null, "events": []}"#,
            r#"{"sid": "short", "events": []}"#,
            r#"{"sid": "AbCdEfGhIjKlMnOpQrStUv", "events": {}}"#,
            r#"{"sid": "AbCdEfGhIjKlMnOpQrStUv", "rid": 5, "events": []}"#,
        ] {
            assert_eq!(parse_batch(body.as_bytes()), Err(Malformed), "{body}");
        }
        let invalid_rid = parse_batch(
            br#"{"sid": "AbCdEfGhIjKlMnOpQrStUv", "rid": "no spaces allowed", "events": []}"#,
        )
        .unwrap();
        assert_eq!(invalid_rid.rid, None);
    }

    #[test]
    fn lines_follow_the_contract_format() {
        let parsed = batch(json!([
            {"ev": "htmx", "t": 1500, "page": "/", "verb": "GET",
             "path": "/fragments/weather?station=KLWV", "status": 200, "ms": 41,
             "rid": "0192f3a0-0000-7000-8000-0000000000bb"},
            {"ev": "click", "t": 2000, "page": "/events", "el": "a", "text": "Open \"the\" events"},
        ]));
        assert_eq!(
            event_line(
                Some("0192f3a0-0000-7000-8000-0000000000aa"),
                SID,
                "203.0.113.9",
                &parsed.events[0]
            ),
            "ui_event site=4casttruth rid=0192f3a0-0000-7000-8000-0000000000aa \
             sid=AbCdEfGhIjKlMnOpQrStUv ip=203.0.113.9 ev=htmx page=/ t=1500 verb=GET \
             path=/fragments/weather status=200 ms=41 hx_rid=0192f3a0-0000-7000-8000-0000000000bb"
        );
        assert_eq!(
            event_line(None, SID, "-", &parsed.events[1]),
            "ui_event site=4casttruth rid=- sid=AbCdEfGhIjKlMnOpQrStUv ip=- ev=click \
             page=/events t=2000 el=a text=\"Open \\\"the\\\" events\""
        );
    }

    #[test]
    fn sessions_and_the_whole_site_are_capped() {
        let start = Instant::now();
        let limits = Limits::new(start);
        // The global bucket holds a second of events.
        assert_eq!(limits.admit(SID, 80, start), 50);
        assert_eq!(limits.admit("another-session-id", 10, start), 0);
        assert_eq!(
            limits.admit("another-session-id", 10, start + Duration::from_millis(200)),
            10
        );

        // A session gets 500 an hour however slowly it sends them.
        let limits = Limits::new(start);
        let mut admitted = 0;
        for second in 0..20 {
            admitted += limits.admit(SID, 40, start + Duration::from_secs(second));
        }
        assert_eq!(admitted, SESSION_EVENTS_PER_HOUR as usize);
        assert_eq!(
            limits.admit(SID, 10, start + Duration::from_secs(30 * 60)),
            0
        );
        assert_eq!(
            limits.admit(SID, 10, start + Duration::from_secs(61 * 60)),
            10
        );
    }
}
