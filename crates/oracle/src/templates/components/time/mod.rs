//! Times as people read them. The server writes UTC and relative text
//! ("12 min ago") with the full timestamp in `title`; `local_time.js` only
//! rewrites absolute times into the reader's time zone, which the server
//! cannot know.

use maud::{Markup, html};
use time::{
    Duration, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339,
    macros::format_description,
};

/// Parses an RFC 3339 timestamp from the weather data.
pub fn parse(timestamp: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(timestamp, &Rfc3339)
        .ok()
        .map(|time| time.to_offset(UtcOffset::UTC))
}

/// "just now", "12 min ago", "3 h ago", "in 2 days"; past a week, the date.
pub fn ago(then: OffsetDateTime, now: OffsetDateTime) -> String {
    let elapsed = now - then;
    let future = elapsed.is_negative();
    let span = elapsed.abs();
    let amount = if span < Duration::minutes(1) {
        return "just now".into();
    } else if span < Duration::hours(1) {
        format!("{} min", span.whole_minutes())
    } else if span < Duration::hours(48) {
        format!("{} h", span.whole_hours())
    } else if span < Duration::days(8) {
        format!("{} days", span.whole_days())
    } else {
        return date(then);
    };
    if future {
        format!("in {amount}")
    } else {
        format!("{amount} ago")
    }
}

/// "Sep 24, 2026 11:44 UTC"
pub fn utc(time: OffsetDateTime) -> String {
    let time = time.to_offset(UtcOffset::UTC);
    time.format(format_description!(
        "[month repr:short] [day padding:none], [year] [hour]:[minute] UTC"
    ))
    .unwrap_or_default()
}

/// "Sep 24"
pub fn date(time: OffsetDateTime) -> String {
    time.to_offset(UtcOffset::UTC)
        .format(format_description!("[month repr:short] [day padding:none]"))
        .unwrap_or_default()
}

/// "Thu, Sep 24" for a `YYYY-MM-DD` calendar day (or a timestamp starting
/// with one). The server has already selected the calendar day, so no conversion applies.
pub fn calendar_day(day: &str) -> String {
    let date = day.split([' ', 'T']).next().unwrap_or(day);
    time::Date::parse(date, format_description!("[year]-[month]-[day]"))
        .ok()
        .and_then(|date| {
            date.format(format_description!(
                "[weekday repr:short], [month repr:short] [day padding:none]"
            ))
            .ok()
        })
        .unwrap_or_else(|| day.to_string())
}

/// Relative text, with the UTC timestamp to hover.
pub fn relative(time: OffsetDateTime, now: OffsetDateTime) -> Markup {
    html! {
        time datetime=(rfc3339(time)) title=(utc(time)) { (ago(time, now)) }
    }
}

/// An absolute time in UTC that the browser rewrites into local time.
pub fn absolute(time: OffsetDateTime) -> Markup {
    html! {
        time class="local-time" datetime=(rfc3339(time)) { (utc(time)) }
    }
}

/// A window on one line: "Sep 24, 11:44–11:54 UTC" or
/// "Sep 24, 09:06 – Sep 25, 03:06 UTC".
pub fn window(start: OffsetDateTime, end: OffsetDateTime) -> Markup {
    html! {
        span class="local-window" data-start=(rfc3339(start)) data-end=(rfc3339(end)) {
            (window_text(start, end))
        }
    }
}

pub fn window_text(start: OffsetDateTime, end: OffsetDateTime) -> String {
    let (start, end) = (
        start.to_offset(UtcOffset::UTC),
        end.to_offset(UtcOffset::UTC),
    );
    let clock = format_description!("[hour]:[minute]");
    let day_clock = format_description!("[month repr:short] [day padding:none], [hour]:[minute]");
    let (Ok(first), Ok(last_clock), Ok(last)) = (
        start.format(day_clock),
        end.format(clock),
        end.format(day_clock),
    ) else {
        return String::new();
    };
    if start.date() == end.date() {
        format!("{first}–{last_clock} UTC")
    } else {
        format!("{first} – {last} UTC")
    }
}

fn rfc3339(time: OffsetDateTime) -> String {
    time.to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn relative_times_read_naturally() {
        let now = datetime!(2026-09-24 12:00 UTC);
        assert_eq!(ago(now - Duration::seconds(20), now), "just now");
        assert_eq!(ago(now - Duration::minutes(12), now), "12 min ago");
        assert_eq!(ago(now - Duration::hours(3), now), "3 h ago");
        assert_eq!(ago(now + Duration::hours(5), now), "in 5 h");
        assert_eq!(ago(now - Duration::days(3), now), "3 days ago");
        assert_eq!(ago(now - Duration::days(30), now), "Aug 25");
    }

    #[test]
    fn windows_fit_on_one_line() {
        let start = datetime!(2026-09-24 11:44 UTC);
        assert_eq!(
            window_text(start, start + Duration::minutes(10)),
            "Sep 24, 11:44–11:54 UTC"
        );
        assert_eq!(
            window_text(start, start + Duration::hours(18)),
            "Sep 24, 11:44 – Sep 25, 05:44 UTC"
        );
    }

    #[test]
    fn calendar_days_stay_on_their_utc_date() {
        assert_eq!(calendar_day("2026-09-20"), "Sun, Sep 20");
        assert_eq!(calendar_day("2026-09-20 00:00:00"), "Sun, Sep 20");
        assert_eq!(calendar_day("not a date"), "not a date");
    }
}
