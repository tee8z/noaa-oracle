# Request logging and browser telemetry

Every HTTP request gets an id, and the oracle writes one line for it. The
pages can also send what readers do on them, as `ui_event` lines. Both are
plain text lines in the usual log format, so the same queries find a visitor
across the proxy, the coordinator and the oracle.

## Request ids and client addresses

The proxy in front of the oracle sets `X-Request-Id` and puts the client's
address in `X-Real-IP`. The oracle believes both only when the TCP peer is
one of `trusted_proxies`. Otherwise it makes its own id (a UUIDv7) and logs
the peer's address. An incoming id must match `^[0-9A-Za-z-]{8,64}$`.

- Every response echoes the id in `X-Request-Id`. Browsers on other sites
  may read it (CORS exposes it).
- `X-Parent-Request-Id`, the calling service's id, is logged as `prid`.
- `X-Session-Id`, the browser tab's session (`^[A-Za-z0-9_-]{16,32}$`), is
  logged as `sid`. Invalid values are logged as `-`.
- Pages carry the id in `<meta name="request-id">`.

`X-Forwarded-For` and `CF-Connecting-IP` are never read.

## Log lines

One line per request, at INFO with target `http`, once the response is
ready:

```text
[2026-10-07T12:00:00.000000000Z INFO] http: http rid=0192f3a0-… prid=- sid=AbCdEfGhIjKlMnOpQrStUv ip=203.0.113.9 method=GET route=/oracle/events/{event_id} status=200 ms=12 user=-
```

- `route` is the matched route template, or the path when nothing matched.
  Query strings, bodies and headers are never logged.
- `user` is the first 16 hex characters of the NIP-98 signer, on requests
  that verified one.
- `/metrics`, `/health`, `/healthy`, `/ready` and `/assets/*` get no line.

Any other line logged while a request is handled ends with ` rid=<id>`.
Work a request hands to a background task is logged without it.

Browser events, one line each, at INFO with target `ui_event`:

```text
[… INFO] ui_event: ui_event site=4casttruth rid=<page rid|-> sid=<sid> ip=<ip> ev=click page=/events t=2000 el=a text="Open events"
```

A value with a space, `"` or `=` is double-quoted, with `"` and `\` escaped.
Control characters are removed, and values are cut to 200 characters.

## Browser telemetry

When `telemetry.enabled` is on, pages carry `<meta name="telemetry"
content="on">` and `telemetry.js` (inside the site script, no extra request)
sends batches to `POST /api/v1/telemetry` with `navigator.sendBeacon`. It
sends batches every 10 seconds while events are waiting, and when the tab is
hidden or the reader leaves the page. A failed batch is not retried.

| `ev` | Fields |
| --- | --- |
| `page_view` | `ref` (the referring path, or the host of another site), `ttfb`, `dcl`, `load` |
| `vitals` | `lcp`, `cls`, `inp` (longest interaction), sent when the reader leaves the page |
| `click` | `el`, `id`, `track` (`data-track`), `text` (buttons and links only, at most 40 characters) |
| `submit` | `form` (the form's id) |
| `htmx` | `verb`, `path` (no query), `status`, `ms`, `rid` (the reply's `X-Request-Id`) |
| `js_error` | `msg`, `src` (file name), `line` |
| `mark` | `name`, from `window.fdcMark(name)` |

The session id is 22 random base64url characters in `sessionStorage`
(`fdc.sid`), for one tab; there are no cookies. htmx requests send it as
`X-Session-Id`. Field values typed by the reader are never read, and nothing
inside `[data-telemetry="off"]` is recorded.

The endpoint takes `application/json` or `text/plain` bodies up to 16 KiB
(413 above that) and answers 204, or 400 for a body that is not a batch, or
404 while telemetry is off. It keeps the first 50 events of a batch, only
known event types and fields with the expected type, and strings without
their query, cut to 200 characters. A string that contains `nsec1`, `npub1`,
`lnbc`, `lntb`, `lnurl`, `xprv` or `tprv` (any case), 40 or more hex digits,
or an email address is logged as `[redacted]`. A session may log 500 events
an hour, and the oracle logs at most 50 a second; events past either are
dropped, still with 204, and counted in
`oracle_telemetry_events_dropped_total`.

## Configuration

```toml
[http_context]
# Proxies whose X-Request-Id and client address header are believed:
# addresses or CIDRs. Empty (the default) trusts none.
trusted_proxies = ["127.0.0.1", "::1"]
client_ip_header = "X-Real-IP"

[telemetry]
enabled = false
```

Flags and environment variables: `--trusted-proxies` /
`NOAA_ORACLE_TRUSTED_PROXIES` (comma separated), `--client-ip-header` /
`NOAA_ORACLE_CLIENT_IP_HEADER`, and `--telemetry-enabled` /
`NOAA_ORACLE_TELEMETRY_ENABLED`. They win over the file.
