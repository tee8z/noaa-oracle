//! The reader's calendar. A line in the page head stores the browser's IANA
//! time zone in the `tz` cookie (for example `America/New_York`), and every
//! htmx request refreshes it. Pages then show the reader's days: today
//! starts at their local midnight, and forecasts and observations are
//! grouped by their local dates, with daylight-saving changes taken from
//! the time zone database. The server works out today from the clock on
//! every request, so a page left open past midnight moves to the new day
//! on its next refresh. Without a usable cookie (a first visit, scripts
//! off, an unknown zone) days are UTC days.

use axum::http::{HeaderMap, HeaderValue, header};

use crate::calendar::Calendar;

/// The cookie holding the reader's IANA time zone.
pub const ZONE_COOKIE: &str = "tz";

/// Longer values are not zone names; IANA names are at most 30 bytes.
const MAX_ZONE_NAME: usize = 64;

/// The value of the request's cookie `name`, if it sent one.
pub(super) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|cookies| cookies.split(';'))
        .filter_map(|cookie| cookie.trim().split_once('='))
        .find(|(cookie, _)| cookie.trim() == name)
        .map(|(_, value)| value.trim())
}

/// The reader's calendar from the `tz` cookie, or UTC.
pub fn reader_calendar(headers: &HeaderMap) -> Calendar {
    cookie(headers, ZONE_COOKIE)
        .filter(|name| name.len() <= MAX_ZONE_NAME)
        .and_then(Calendar::from_zone_name)
        .unwrap_or_default()
}

/// Responses that depend on the reader's cookies (their calendar, their
/// remembered view) must not be served from a cache to another reader.
pub(super) fn vary_on_cookie(headers: &mut axum::http::HeaderMap) {
    let vary = headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if vary
        .iter()
        .any(|value| value.eq_ignore_ascii_case("cookie"))
    {
        return;
    }
    let value = vary
        .into_iter()
        .chain(["Cookie".to_owned()])
        .collect::<Vec<_>>()
        .join(", ");
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(header::VARY, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_cookie(cookie: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        headers
    }

    #[test]
    fn the_zone_cookie_sets_the_readers_calendar_and_anything_else_means_utc() {
        assert_eq!(
            reader_calendar(&with_cookie("weather_view=list; tz=America/New_York")).name(),
            "America/New_York"
        );
        assert_eq!(
            reader_calendar(&with_cookie("tz=Asia/Kolkata")).name(),
            "Asia/Kolkata"
        );
        for ignored in [
            "tz=",
            "tz=Mars/Olympus",
            "tz=America/New_York%3B",
            "tz=UTC",
            "other=America/New_York",
        ] {
            assert_eq!(
                reader_calendar(&with_cookie(ignored)),
                Calendar::Utc,
                "{ignored}"
            );
        }
        let long = format!("tz={}", "A/".repeat(40));
        assert_eq!(reader_calendar(&with_cookie(&long)), Calendar::Utc);
        assert_eq!(reader_calendar(&HeaderMap::new()), Calendar::Utc);
    }

    #[test]
    fn cookies_are_read_by_exact_name() {
        let headers = with_cookie("xtz=Europe/Paris; tz = Europe/Berlin ; weather_view=map");
        assert_eq!(cookie(&headers, "tz"), Some("Europe/Berlin"));
        assert_eq!(cookie(&headers, "weather_view"), Some("map"));
        assert_eq!(cookie(&headers, "missing"), None);
    }

    #[test]
    fn vary_gains_cookie_once() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::VARY,
            HeaderValue::from_static("HX-Request, HX-Target"),
        );
        vary_on_cookie(&mut headers);
        vary_on_cookie(&mut headers);
        assert_eq!(headers[header::VARY], "HX-Request, HX-Target, Cookie");
        let mut empty = HeaderMap::new();
        vary_on_cookie(&mut empty);
        assert_eq!(empty[header::VARY], "Cookie");
    }
}
