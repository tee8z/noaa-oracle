//! Calendar days: UTC days, or a reader's local days in their IANA time
//! zone with its daylight-saving changes.

use chrono::{LocalResult, NaiveDate, Offset, TimeZone};
use chrono_tz::Tz;
use time::{Date, Duration, OffsetDateTime, UtcOffset};

/// How instants are grouped into calendar days.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Calendar {
    #[default]
    Utc,
    Zone(Tz),
}

impl Calendar {
    /// The calendar of an IANA time zone name such as `America/New_York`.
    /// Unknown names give `None`. Zones that are UTC all year give
    /// [`Calendar::Utc`], so they share its cached pages.
    pub fn from_zone_name(name: &str) -> Option<Self> {
        let zone: Tz = name.parse().ok()?;
        let always_utc = [2026, 2027].iter().all(|year| {
            [1, 7].iter().all(|month| {
                NaiveDate::from_ymd_opt(*year, *month, 1)
                    .and_then(|date| date.and_hms_opt(0, 0, 0))
                    .map(|moment| {
                        zone.offset_from_utc_datetime(&moment)
                            .fix()
                            .local_minus_utc()
                    })
                    == Some(0)
            })
        });
        Some(if always_utc {
            Self::Utc
        } else {
            Self::Zone(zone)
        })
    }

    /// The zone's name, `UTC` for UTC days.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Utc => "UTC",
            Self::Zone(zone) => zone.name(),
        }
    }

    /// How pages name these days: `UTC`, or the zone's place, such as
    /// `New York time`.
    pub fn place(&self) -> String {
        match self {
            Self::Utc => "UTC".to_owned(),
            Self::Zone(zone) => {
                let place = zone.name().rsplit('/').next().unwrap_or(zone.name());
                format!("{} time", place.replace('_', " "))
            }
        }
    }

    /// The offset from UTC in force at `instant`.
    pub fn offset_at(&self, instant: OffsetDateTime) -> UtcOffset {
        let Self::Zone(zone) = self else {
            return UtcOffset::UTC;
        };
        let seconds = zone
            .offset_from_utc_datetime(
                &chrono::DateTime::from_timestamp(instant.unix_timestamp(), 0)
                    .unwrap_or_default()
                    .naive_utc(),
            )
            .fix()
            .local_minus_utc();
        UtcOffset::from_whole_seconds(seconds).unwrap_or(UtcOffset::UTC)
    }

    /// The calendar date `instant` falls on.
    pub fn date_of(&self, instant: OffsetDateTime) -> Date {
        instant.to_offset(self.offset_at(instant)).date()
    }

    /// The first instant of `date`: its midnight, or when a daylight-saving
    /// change skips midnight, the first local time after the gap.
    pub fn start_of(&self, date: Date) -> OffsetDateTime {
        let utc_midnight = date.midnight().assume_utc();
        let Self::Zone(zone) = self else {
            return utc_midnight;
        };
        let Some(naive) = NaiveDate::from_ymd_opt(
            date.year(),
            u32::from(u8::from(date.month())),
            u32::from(date.day()),
        )
        .and_then(|day| day.and_hms_opt(0, 0, 0)) else {
            return utc_midnight;
        };
        // A gap is at most a few hours; step through it a quarter hour at a time.
        let mut local = naive;
        for _ in 0..=16 {
            match zone.from_local_datetime(&local) {
                LocalResult::Single(moment) | LocalResult::Ambiguous(moment, _) => {
                    return OffsetDateTime::from_unix_timestamp(moment.timestamp())
                        .unwrap_or(utc_midnight);
                }
                LocalResult::None => local += chrono::Duration::minutes(15),
            }
        }
        utc_midnight
    }

    /// When the calendar day holding `instant` began.
    pub fn start_of_day(&self, instant: OffsetDateTime) -> OffsetDateTime {
        self.start_of(self.date_of(instant))
    }

    /// SQL giving the calendar day of the instant `expression` as
    /// `YYYY-MM-DD 00:00:00`. For UTC it is exact for every instant; for a
    /// zone, for instants from two days before `from` to a day after `to`,
    /// with the offset in force two days before `from` used before that.
    pub(crate) fn day_sql(
        &self,
        expression: &str,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> String {
        let Self::Zone(_) = self else {
            return format!("DATE_TRUNC('day', {expression} AT TIME ZONE 'UTC')::VARCHAR");
        };
        let first = self.date_of(from) - Duration::days(2);
        let last = self.date_of(to) + Duration::days(1);
        let mut days = vec![];
        let mut day = first;
        while day <= last {
            days.push((day, self.start_of(day)));
            day = day.next_day().unwrap_or(day);
            if days.len() > 64 {
                break;
            }
        }
        let before = self.offset_at(days[0].1).whole_seconds();
        let mut sql = String::from("CASE");
        for (day, start) in days.iter().rev() {
            sql.push_str(&format!(
                " WHEN {expression} >= to_timestamp({}) THEN '{} 00:00:00'",
                start.unix_timestamp(),
                iso_date(*day)
            ));
        }
        sql.push_str(&format!(
            " ELSE DATE_TRUNC('day', ({expression} AT TIME ZONE 'UTC') + INTERVAL '{before} seconds')::VARCHAR END"
        ));
        sql
    }
}

fn iso_date(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime};

    fn new_york() -> Calendar {
        Calendar::from_zone_name("America/New_York").unwrap()
    }

    #[test]
    fn zone_names_are_checked_and_utc_zones_share_the_utc_calendar() {
        assert_eq!(new_york().name(), "America/New_York");
        for utc in ["UTC", "Etc/UTC", "Etc/GMT", "Africa/Abidjan"] {
            assert_eq!(Calendar::from_zone_name(utc), Some(Calendar::Utc), "{utc}");
        }
        for unknown in ["", "Mars/Olympus", "America/New_York; x=1", "../etc/passwd"] {
            assert_eq!(Calendar::from_zone_name(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn days_are_named_by_their_place() {
        assert_eq!(Calendar::Utc.place(), "UTC");
        assert_eq!(new_york().place(), "New York time");
        assert_eq!(
            Calendar::from_zone_name("America/Argentina/Buenos_Aires")
                .unwrap()
                .place(),
            "Buenos Aires time"
        );
    }

    #[test]
    fn local_days_follow_daylight_saving_changes() {
        let calendar = new_york();
        // Summer: UTC-4.
        assert_eq!(
            calendar.start_of(date!(2026 - 09 - 24)),
            datetime!(2026-09-24 04:00 UTC)
        );
        // Winter: UTC-5.
        assert_eq!(
            calendar.start_of(date!(2026 - 11 - 02)),
            datetime!(2026-11-02 05:00 UTC)
        );
        // 01:30 UTC is still the previous evening in New York.
        assert_eq!(
            calendar.start_of_day(datetime!(2026-09-25 01:30 UTC)),
            datetime!(2026-09-24 04:00 UTC)
        );
        // The fall-back day has 25 hours: 04:30 UTC on November 2 is still November 1.
        assert_eq!(
            calendar.date_of(datetime!(2026-11-02 04:30 UTC)),
            date!(2026 - 11 - 01)
        );
        assert_eq!(
            Calendar::Utc.start_of_day(datetime!(2026-09-25 01:30 UTC)),
            datetime!(2026-09-25 00:00 UTC)
        );
    }

    #[test]
    fn a_skipped_midnight_starts_the_day_after_the_gap() {
        // Santiago springs forward at midnight: 2026-09-06 00:00 does not exist.
        let calendar = Calendar::from_zone_name("America/Santiago").unwrap();
        let start = calendar.start_of(date!(2026 - 09 - 06));
        assert_eq!(calendar.date_of(start), date!(2026 - 09 - 06));
        assert_eq!(
            calendar.date_of(start - Duration::seconds(1)),
            date!(2026 - 09 - 05)
        );
    }
}
