//! The reader's "today". A one-line script in the page head stores the
//! browser's UTC offset, in minutes east of UTC, in the `utc_offset` cookie.
//! Pages then take today to be the reader's calendar day: observations from
//! their local midnight, and forecasts grouped by their local dates. Without
//! the cookie (a first visit, or scripts turned off) days are UTC days.

use axum::http::{HeaderMap, header};
use time::{OffsetDateTime, Time, UtcOffset};

pub const OFFSET_COOKIE: &str = "utc_offset";

/// Real offsets lie within ±14 hours; anything else is ignored.
const MAX_OFFSET_MINUTES: i32 = 14 * 60;

/// The reader's UTC offset from the `utc_offset` cookie, or UTC.
pub fn reader_offset(headers: &HeaderMap) -> UtcOffset {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|cookies| cookies.split(';'))
        .filter_map(|cookie| cookie.trim().split_once('='))
        .find(|(name, _)| *name == OFFSET_COOKIE)
        .and_then(|(_, minutes)| minutes.trim().parse::<i32>().ok())
        .filter(|minutes| (-MAX_OFFSET_MINUTES..=MAX_OFFSET_MINUTES).contains(minutes))
        .and_then(|minutes| UtcOffset::from_whole_seconds(minutes * 60).ok())
        .unwrap_or(UtcOffset::UTC)
}

/// The midnight that started the reader's current day.
pub fn start_of_today(now: OffsetDateTime, offset: UtcOffset) -> OffsetDateTime {
    now.to_offset(offset).replace_time(Time::MIDNIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use time::macros::{datetime, offset};

    fn with_cookie(cookie: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        headers
    }

    #[test]
    fn the_offset_cookie_sets_the_readers_day_and_anything_else_means_utc() {
        assert_eq!(
            reader_offset(&with_cookie("weather_view=list; utc_offset=-240")),
            offset!(-4)
        );
        assert_eq!(
            reader_offset(&with_cookie("utc_offset=330")),
            offset!(+5:30)
        );
        for ignored in [
            "utc_offset=abc",
            "utc_offset=1000",
            "utc_offset=",
            "utc_offset=-2147483648",
            "other=60",
        ] {
            assert_eq!(
                reader_offset(&with_cookie(ignored)),
                UtcOffset::UTC,
                "{ignored}"
            );
        }
        assert_eq!(reader_offset(&HeaderMap::new()), UtcOffset::UTC);
    }

    #[test]
    fn today_starts_at_the_readers_midnight() {
        // 01:30 UTC is still the previous evening in New York.
        let now = datetime!(2026-09-25 01:30 UTC);
        assert_eq!(
            start_of_today(now, offset!(-4)),
            datetime!(2026-09-24 00:00 -4)
        );
        assert_eq!(
            start_of_today(now, UtcOffset::UTC),
            datetime!(2026-09-25 00:00 UTC)
        );
    }
}
