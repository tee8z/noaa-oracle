//! Which stations a competition can be drawn from now: those whose reports
//! would have met settlement's sampling rule on nearly every recent day,
//! that are still reporting, and whose newest forecast runs past the
//! competition window.
//!
//! One query lists the reports of the period that settlement reads.
//! Rejected reports are left out, and so are reports from files written
//! before the daemon's quality fields, which settlement refuses. An
//! unverified report stays a report, and its temperature drops out when its
//! problems affect temperatures, as in settlement. A second query, on the
//! same connection, finds how far each station's newest forecast issue
//! runs. The days are then judged with settlement's own sampling rule.
//!
//! Reading the reports is the slow part, seconds over every station, and
//! judging them takes milliseconds. So the reports of the last
//! [`PRECOMPUTED_DAYS`] days are read ahead of requests after each
//! collection run and kept as a [`Timeline`], and any shorter history or
//! window length is judged from it without reading the files again.

use super::{
    DEDUP_OBSERVATIONS_SQL, Error, NORMALIZE_OBSERVATIONS_SQL, OBSERVATION_SOURCE_COLUMNS,
    WeatherAccess, forecast_file_params, integer, integers, precipitation, sql_string_list,
    station, strings,
};
use crate::file_access::FileParams;
use duckdb::{Connection, arrow::array::RecordBatch};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError},
};
use time::{Duration, OffsetDateTime, Time, UtcOffset, format_description::well_known::Rfc3339};
use utoipa::ToSchema;

/// Full UTC days judged when a query names none. Longer histories take in
/// more collection outages and judge few stations differently.
pub const DEFAULT_DAYS: u32 = 3;
/// Days of reports read ahead of requests (see [`Timeline`]).
pub const PRECOMPUTED_DAYS: u32 = DEFAULT_DAYS;
/// Most days one query may judge: the longest public time range.
pub const MAX_DAYS: u32 = 31;
/// Competition window length when a query names none.
pub const DEFAULT_WINDOW_HOURS: u32 = 24;
/// Longest window: one placed on a day must fit in it.
pub const MAX_WINDOW_HOURS: u32 = 24;
/// Imperfect days allowed among those checked: one in ten, and always at least one, so a
/// single collector outage does not empty the list.
fn allowed_imperfect_days(days_checked: u32) -> u32 {
    (days_checked / 10).max(1)
}
/// A judged day on which fewer than half of the stations were clean was a
/// gap in collection, not at the stations, and is not checked against
/// them. Below this many stations one station's bad day cannot be told
/// from an outage, so every day is checked.
const OUTAGE_MIN_STATIONS: usize = 20;
/// How long reports read ahead serve requests. Collection runs hourly and
/// each run reads them again; past this the files are read per request.
const TIMELINE_MAX_AGE: Duration = Duration::minutes(90);
/// A station whose newest report is this old may have stopped reporting.
const MAX_REPORT_AGE: Duration = Duration::hours(3);
/// Forecast issues this recent are read. A station whose newest issue is
/// older has no forecast to open a competition with.
const FORECAST_ISSUES: Duration = Duration::hours(6);

/// One report settlement reads, and whether its temperature is usable.
#[derive(Clone, Copy, Debug)]
pub(super) struct Report {
    pub time: OffsetDateTime,
    pub temperature: bool,
}

/// How one station's recent reports and forecasts measure up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eligibility {
    pub station_id: String,
    /// Judged days on which every window was sampled.
    pub clean_days: u32,
    pub days_checked: u32,
    /// The newest report settlement reads.
    pub last_report: OffsetDateTime,
    /// End of the last period of the station's newest forecast issue, if
    /// it has a recent one.
    pub forecast_through: Option<OffsetDateTime>,
    /// Clean on enough days, reporting, and forecast past the window.
    pub eligible: bool,
}

/// A station a competition can be drawn from, as `/stations/eligible`
/// lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct EligibleStation {
    pub station_id: String,
    pub station_name: String,
    pub state: String,
    pub iata_id: String,
    pub latitude: f64,
    pub longitude: f64,
    /// Days among the judged ones on which the station's reports covered
    /// every window of the requested length.
    pub clean_days: u32,
    pub days_checked: u32,
    /// Time of the newest report (RFC3339).
    pub last_report: String,
    /// End of the newest forecast's last period (RFC3339).
    pub forecast_through: String,
}

impl EligibleStation {
    /// `station` as listed, when `eligibility` has a forecast.
    pub fn new(station: &super::Station, eligibility: &Eligibility) -> Option<Self> {
        Some(Self {
            station_id: station.station_id.clone(),
            station_name: station.station_name.clone(),
            state: station.state.clone(),
            iata_id: station.iata_id.clone(),
            latitude: station.latitude,
            longitude: station.longitude,
            clean_days: eligibility.clean_days,
            days_checked: eligibility.days_checked,
            last_report: eligibility.last_report.format(&Rfc3339).ok()?,
            forecast_through: eligibility.forecast_through?.format(&Rfc3339).ok()?,
        })
    }
}

/// The `days` full UTC days before the one `now` falls on, oldest first.
fn judged_days(days: u32, now: OffsetDateTime) -> Vec<OffsetDateTime> {
    let today = now.to_offset(UtcOffset::UTC).replace_time(Time::MIDNIGHT);
    (1..=i64::from(days))
        .rev()
        .map(|back| today - Duration::days(back))
        .collect()
}

/// How many of `days` (UTC midnights) are clean (see [`clean_day`]).
/// `reports` are one station's, in time order.
#[cfg(test)]
pub(super) fn clean_days(reports: &[Report], days: &[OffsetDateTime], window_hours: u32) -> u32 {
    days.iter()
        .filter(|day| clean_day(reports, **day, window_hours))
        .count() as u32
}

/// Whether every window of `window_hours` that starts on the hour and ends
/// within `day` (a UTC midnight) is sampled, both by all of `reports` and
/// by those with a usable temperature. `reports` are one station's, in
/// time order. Competitions can start at any hour, so a gap anywhere in
/// the day counts against it.
fn clean_day(reports: &[Report], day: OffsetDateTime, window_hours: u32) -> bool {
    let window = Duration::hours(i64::from(window_hours));
    let from = reports.partition_point(|report| report.time < day);
    let to = reports.partition_point(|report| report.time < day + Duration::DAY);
    let reports = &reports[from..to];
    (0..=i64::from(MAX_WINDOW_HOURS.saturating_sub(window_hours))).all(|hour| {
        let start = day + Duration::hours(hour);
        let end = start + window;
        precipitation::times_sample_window(reports.iter().map(|report| report.time), start, end)
            && precipitation::times_sample_window(
                reports
                    .iter()
                    .filter(|report| report.temperature)
                    .map(|report| report.time),
                start,
                end,
            )
    })
}

/// Which of the judged days were collection outages: days most stations
/// missed, by each station's clean days in `clean`.
fn outage_days(clean: &[Vec<bool>], days: usize) -> Vec<bool> {
    if clean.len() < OUTAGE_MIN_STATIONS {
        return vec![false; days];
    }
    (0..days)
        .map(|day| clean.iter().filter(|station| station[day]).count() * 2 < clean.len())
        .collect()
}

/// The reports settlement reads and each station's forecast extent, from
/// the full UTC days before the one they were read on until then.
pub(super) struct Timeline {
    read_at: OffsetDateTime,
    days: u32,
    /// Each station's reports, in time order.
    reports: BTreeMap<String, Vec<Report>>,
    forecasts: BTreeMap<String, Option<OffsetDateTime>>,
}

impl Timeline {
    /// Whether it holds every report a judgment of `days` at `now` reads:
    /// read the same UTC day, recently, over at least as many days.
    fn covers(&self, days: u32, now: OffsetDateTime) -> bool {
        days <= self.days
            && now >= self.read_at
            && now - self.read_at < TIMELINE_MAX_AGE
            && now.to_offset(UtcOffset::UTC).date() == self.read_at.to_offset(UtcOffset::UTC).date()
    }

    pub(super) fn estimated_bytes(&self) -> usize {
        self.reports
            .iter()
            .map(|(id, reports)| id.capacity() + reports.capacity() * std::mem::size_of::<Report>())
            .sum::<usize>()
            + self
                .forecasts
                .keys()
                .map(|id| id.capacity() + std::mem::size_of::<Option<OffsetDateTime>>())
                .sum::<usize>()
    }

    /// Reports held, over every station.
    pub(super) fn report_count(&self) -> usize {
        self.reports.values().map(Vec::len).sum()
    }
}

/// Judges every station that reported in the last `days` full UTC days
/// before `now` for a competition of `window_hours` starting at `now`.
/// Ordered by station id.
fn judge(
    timeline: &Timeline,
    days: u32,
    window_hours: u32,
    now: OffsetDateTime,
) -> Vec<Eligibility> {
    let days = judged_days(days, now);
    let Some(first) = days.first().copied() else {
        return vec![];
    };
    let stations: Vec<(&String, &[Report], Vec<bool>)> = timeline
        .reports
        .iter()
        .filter_map(|(station_id, reports)| {
            let reports = &reports[reports.partition_point(|report| report.time < first)..];
            if reports.is_empty() {
                return None;
            }
            let clean = days
                .iter()
                .map(|day| clean_day(reports, *day, window_hours))
                .collect();
            Some((station_id, reports, clean))
        })
        .collect();
    let clean: Vec<Vec<bool>> = stations.iter().map(|(_, _, clean)| clean.clone()).collect();
    let outages = outage_days(&clean, days.len());
    let days_checked = outages.iter().filter(|outage| !**outage).count() as u32;
    stations
        .into_iter()
        .filter_map(|(station_id, reports, clean)| {
            let last_report = reports.last()?.time;
            let clean_days = clean
                .iter()
                .zip(&outages)
                .filter(|(clean, outage)| **clean && !**outage)
                .count() as u32;
            let forecast_through = timeline.forecasts.get(station_id).copied().flatten();
            Some(Eligibility {
                eligible: eligible(
                    clean_days,
                    days_checked,
                    last_report,
                    forecast_through,
                    window_hours,
                    now,
                ),
                station_id: station_id.clone(),
                clean_days,
                days_checked,
                last_report,
                forecast_through,
            })
        })
        .collect()
}

/// Whether a station with these figures can host a competition of
/// `window_hours` starting at `now`.
fn eligible(
    clean_days: u32,
    days_checked: u32,
    last_report: OffsetDateTime,
    forecast_through: Option<OffsetDateTime>,
    window_hours: u32,
    now: OffsetDateTime,
) -> bool {
    days_checked > 0
        && clean_days + allowed_imperfect_days(days_checked) >= days_checked
        && now - last_report < MAX_REPORT_AGE
        && forecast_through
            .is_some_and(|through| through >= now + Duration::hours(i64::from(window_hours)))
}

/// Each station's instants, with the raw epoch seconds kept for ordering.
type StationTimes = BTreeMap<String, Vec<(OffsetDateTime, Option<i64>)>>;

/// Instants of an epoch-seconds column, by station.
fn times_by_station(
    batches: &[RecordBatch],
    time_column: &'static str,
) -> Result<StationTimes, Error> {
    let mut by_station = BTreeMap::<String, Vec<_>>::new();
    for batch in batches {
        let stations = strings(batch, "station_id")?;
        let times = integers(batch, time_column)?;
        let flags = integers(batch, "flag")?;
        for row in 0..batch.num_rows() {
            let (Some(station_id), Some(time)) = (
                station(stations, row),
                integer(times, row)
                    .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok()),
            ) else {
                continue;
            };
            by_station
                .entry(station_id)
                .or_default()
                .push((time, integer(flags, row)));
        }
    }
    Ok(by_station)
}

impl WeatherAccess {
    /// Every station that reported in the last `days` full UTC days, and
    /// whether a competition of `window_hours` starting at `now` can be
    /// drawn from it. Ordered by station id. Judged from the reports read
    /// ahead when they cover the days, from the files otherwise.
    pub(super) async fn eligibility(
        &self,
        days: u32,
        window_hours: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<Eligibility>, Error> {
        // Whole seconds, as the reports are.
        let now = now - Duration::nanoseconds(now.nanosecond().into());
        let read_ahead = self
            .timeline
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|timeline| timeline.covers(days, now));
        let timeline = match read_ahead {
            Some(timeline) => timeline,
            None => Arc::new(self.read_timeline(days, now).await?),
        };
        Ok(tokio::task::spawn_blocking(move || judge(&timeline, days, window_hours, now)).await?)
    }

    /// Reads the reports of the last [`PRECOMPUTED_DAYS`] days for later
    /// judgments, and returns how many it holds.
    pub(super) async fn read_ahead_reports(&self, now: OffsetDateTime) -> Result<usize, Error> {
        let now = now - Duration::nanoseconds(now.nanosecond().into());
        let timeline = self.read_timeline(PRECOMPUTED_DAYS, now).await?;
        let reports = timeline.report_count();
        let held = if timeline.estimated_bytes() <= 128 * 1024 * 1024 {
            Some(Arc::new(timeline))
        } else {
            None
        };
        let count = if held.is_some() { reports } else { 0 };
        *self.timeline.lock().unwrap_or_else(PoisonError::into_inner) = held;
        Ok(count)
    }

    /// Reads the reports of the `days` full UTC days before `now` and the
    /// forecast extents, as of `now` in whole seconds.
    async fn read_timeline(&self, days: u32, now: OffsetDateTime) -> Result<Timeline, Error> {
        let empty = || Timeline {
            read_at: now,
            days,
            reports: BTreeMap::new(),
            forecasts: BTreeMap::new(),
        };
        let judged = judged_days(days, now);
        let Some(first) = judged.first().copied() else {
            return Ok(empty());
        };
        let names = self
            .file_access
            .grab_file_names(FileParams {
                start: Some(first - Duration::DAY),
                end: Some(now.to_offset(UtcOffset::UTC)),
                observations: Some(true),
                forecasts: Some(false),
            })
            .await?;
        let files = self.file_access.build_file_paths(names);
        if files.is_empty() {
            return Ok(empty());
        }
        // Screening compares each report with the one before it, so read
        // from a little earlier than the first judged day.
        let read_from = (first - Duration::hours(2)).format(&Rfc3339)?;
        let first = first.format(&Rfc3339)?;
        let until = now.format(&Rfc3339)?;
        let reports_sql = format!(
            r#"
            WITH parquet_data AS (
                SELECT * FROM (
                    {OBSERVATION_SOURCE_COLUMNS}
                    UNION ALL BY NAME
                    SELECT * FROM read_parquet([{}], union_by_name = true, filename = true)
                )
                WHERE generated_at::TIMESTAMPTZ >= '{read_from}'::TIMESTAMPTZ
                    AND generated_at::TIMESTAMPTZ <= '{until}'::TIMESTAMPTZ
            ),
            deduped AS ({DEDUP_OBSERVATIONS_SQL}),
            normalized AS ({NORMALIZE_OBSERVATIONS_SQL})
            SELECT station_id::VARCHAR AS station_id,
                epoch(generated_at::TIMESTAMPTZ)::BIGINT AS report_time,
                (temperature_value IS NOT NULL)::BIGINT AS flag
            FROM normalized
            WHERE NOT qc_rejected
                AND NOT (quality_status IS NULL AND raw_text IS NULL)
                AND generated_at::TIMESTAMPTZ >= '{first}'::TIMESTAMPTZ
            ORDER BY station_id, report_time
            "#,
            sql_string_list(&files)
        );

        let issued = (now - FORECAST_ISSUES, now);
        let issued_from = issued.0.format(&Rfc3339)?;
        let forecast_names = self
            .file_access
            .grab_file_names(forecast_file_params(issued, now))
            .await?;
        let forecasts_sql = (!forecast_names.is_empty()).then(|| {
            let conditions = format!(
                "generated_ts >= '{issued_from}'::TIMESTAMPTZ AND generated_ts <= '{until}'::TIMESTAMPTZ \
                 AND end_ts > '{until}'::TIMESTAMPTZ \
                 AND quality_status IS DISTINCT FROM 'rejected'"
            );
            let rows = self.forecast_rows_sql(forecast_names, issued, None, &conditions);
            format!(
                r#"
                SELECT station_id::VARCHAR AS station_id,
                    epoch(MAX(end_ts))::BIGINT AS forecast_through, NULL::BIGINT AS flag
                FROM ({rows})
                GROUP BY station_id, generated_ts
                QUALIFY ROW_NUMBER() OVER (PARTITION BY station_id ORDER BY generated_ts DESC) = 1
                "#
            )
        });

        let decode = move |connection: &Connection,
                           batches: &[RecordBatch]|
              -> Result<Vec<Timeline>, Error> {
            let reports: BTreeMap<String, Vec<Report>> = times_by_station(batches, "report_time")?
                .into_iter()
                .map(|(station_id, rows)| {
                    let reports: Vec<Report> = rows
                        .into_iter()
                        .map(|(time, flag)| Report {
                            time,
                            temperature: flag == Some(1),
                        })
                        .collect();
                    (station_id, reports)
                })
                .collect();
            let mut forecasts = BTreeMap::new();
            if let Some(sql) = forecasts_sql {
                let mut statement = connection.prepare(&sql)?;
                let batches: Vec<RecordBatch> = statement.query_arrow([])?.collect();
                for (station_id, times) in times_by_station(&batches, "forecast_through")? {
                    forecasts.insert(station_id, times.into_iter().map(|(time, _)| time).max());
                }
            }
            Ok(vec![Timeline {
                read_at: now,
                days,
                reports,
                forecasts,
            }])
        };
        // Weeks of reports through the window functions that deduplicate
        // and screen them filled the pool queries share and more beside it.
        // A history longer than the days read ahead is read in a smaller
        // database of its own instead (see `query_alone`).
        let mut timelines = if days > PRECOMPUTED_DAYS {
            self.query_alone(reports_sql, decode).await?
        } else {
            self.query_with_connection(reports_sql, decode).await?
        };
        Ok(timelines.pop().unwrap_or_else(empty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::weather_data::open_connection;
    use std::sync::Arc;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-20 12:00 UTC);

    /// Hourly reports at minute 53 over `days` from 2026-01-15, without the
    /// hours in `missed`, all with temperatures except at `unverified`.
    fn hourly(days: i64, missed: &[i64], unverified: &[i64]) -> Vec<Report> {
        let start = datetime!(2026-01-15 00:53 UTC);
        (0..days * 24)
            .filter(|hour| !missed.contains(&(hour % 24)))
            .map(|hour| Report {
                time: start + Duration::hours(hour),
                temperature: !unverified.contains(&(hour % 24)),
            })
            .collect()
    }

    #[test]
    fn judged_days_are_the_full_utc_days_before_today() {
        assert_eq!(
            judged_days(2, datetime!(2026-01-20 02:00 +05:00)),
            vec![
                datetime!(2026-01-17 00:00 UTC),
                datetime!(2026-01-18 00:00 UTC)
            ]
        );
        assert!(judged_days(0, NOW).is_empty());
    }

    #[test]
    fn a_day_is_clean_when_every_window_on_it_is_sampled() {
        let days = judged_days(5, NOW);
        assert_eq!(clean_days(&hourly(5, &[], &[]), &days, 24), 5);
        // A long window tolerates one missed report, a short one none.
        assert_eq!(clean_days(&hourly(5, &[10], &[]), &days, 24), 5);
        assert_eq!(clean_days(&hourly(5, &[10], &[]), &days, 6), 0);
        // Two missed reports in a row leave a three-hour gap.
        assert_eq!(clean_days(&hourly(5, &[10, 11], &[]), &days, 24), 0);
        // So do unusable temperatures, for the temperature reports.
        assert_eq!(clean_days(&hourly(5, &[10], &[11]), &days, 24), 0);
        // Days without reports are not clean.
        assert_eq!(clean_days(&hourly(3, &[], &[]), &days, 24), 3);
    }

    #[test]
    fn eligibility_needs_clean_days_a_recent_report_and_a_forecast() {
        let through = Some(NOW + Duration::hours(24));
        let recent = NOW - Duration::minutes(10);
        assert!(eligible(29, 30, recent, through, 24, NOW));
        assert!(eligible(19, 20, recent, through, 24, NOW));
        assert!(eligible(27, 30, recent, through, 24, NOW));
        assert!(!eligible(26, 30, recent, through, 24, NOW));
        assert!(eligible(2, 3, recent, through, 24, NOW));
        assert!(!eligible(1, 3, recent, through, 24, NOW));
        assert!(!eligible(30, 30, NOW - MAX_REPORT_AGE, through, 24, NOW));
        assert!(!eligible(30, 30, recent, through, 25, NOW));
        assert!(!eligible(30, 30, recent, None, 24, NOW));
        assert!(!eligible(0, 0, recent, through, 24, NOW));
    }

    /// Writes `select` as the daemon file `name` under `directory`.
    fn write(directory: &tempfile::TempDir, name: &str, select: &str) {
        let file = crate::file_access::ParquetFileName::parse(name).unwrap();
        let day = directory.path().join(file.generated_at.date().to_string());
        std::fs::create_dir_all(&day).unwrap();
        open_connection()
            .unwrap()
            .execute_batch(&format!(
                "COPY ({select}) TO '{}' (FORMAT PARQUET)",
                day.join(name).display()
            ))
            .unwrap();
    }

    /// KCLN reports every hour; KGAP misses its 02:53 report every day and
    /// its 03:53 report is rejected, a three-hour gap; KNOF reports every
    /// hour but its only forecast is a day old.
    #[tokio::test]
    async fn stations_with_gaps_or_no_forecast_are_not_eligible() {
        let directory = tempfile::tempdir().unwrap();
        write(
            &directory,
            "observations_2026-01-20T11:55:00Z.parquet",
            "SELECT station_id, strftime(ts, '%Y-%m-%dT%H:%M:%SZ') AS generated_at,
                    10.0::DOUBLE AS temperature_value, 'celsius' AS temperature_unit_code,
                    'METAR' AS metar_type,
                    'METAR ' || station_id || ' ' || strftime(ts, '%d%H%MZ') || ' 09008KT 10SM 10/08 RMK AO2' AS raw_text,
                    CASE WHEN station_id = 'KGAP' AND hour(ts) = 3 THEN 'rejected' ELSE 'validated' END AS quality_status,
                    'metar-consistency-v1' AS validation_version,
                    CASE WHEN station_id = 'KGAP' AND hour(ts) = 3 THEN 'flagged' END AS quality_reason,
                    CASE WHEN station_id = 'KGAP' AND hour(ts) = 3 THEN 'temperature' END AS quality_metrics
             FROM (VALUES ('KCLN'), ('KGAP'), ('KNOF')) AS stations(station_id),
                  generate_series(TIMESTAMP '2026-01-13 00:53:00', TIMESTAMP '2026-01-20 11:53:00', INTERVAL 1 HOUR) AS hours(ts)
             WHERE NOT (station_id = 'KGAP' AND hour(ts) = 2)",
        );
        let forecast = |station: &str, issued: &str, end: &str| {
            format!(
                "SELECT '{station}' AS station_id, begin_time, end_time, '{issued}' AS generated_at,
                        30::BIGINT AS min_temp, 40::BIGINT AS max_temp, 'fahrenheit' AS temperature_unit_code
                 FROM (VALUES ('2026-01-20T12:00:00Z', '2026-01-21T00:00:00Z'),
                              ('2026-01-21T00:00:00Z', '{end}')) AS periods(begin_time, end_time)"
            )
        };
        write(
            &directory,
            "forecasts_2026-01-20T11:30:00Z.parquet",
            &format!(
                "{} UNION ALL {}",
                forecast("KCLN", "2026-01-20T11:00:00Z", "2026-01-21T18:00:00Z"),
                forecast("KGAP", "2026-01-20T11:00:00Z", "2026-01-21T18:00:00Z")
            ),
        );
        write(
            &directory,
            "forecasts_2026-01-19T12:05:00Z.parquet",
            &forecast("KNOF", "2026-01-19T12:00:00Z", "2026-01-22T00:00:00Z"),
        );
        let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
            directory.path().to_string_lossy().into_owned(),
        )));

        let stations = access.eligibility(5, 24, NOW).await.unwrap();
        let last_report = datetime!(2026-01-20 11:53 UTC);
        let through = datetime!(2026-01-21 18:00 UTC);
        assert_eq!(
            stations,
            vec![
                Eligibility {
                    station_id: "KCLN".into(),
                    clean_days: 5,
                    days_checked: 5,
                    last_report,
                    forecast_through: Some(through),
                    eligible: true,
                },
                Eligibility {
                    station_id: "KGAP".into(),
                    clean_days: 0,
                    days_checked: 5,
                    last_report,
                    forecast_through: Some(through),
                    eligible: false,
                },
                Eligibility {
                    station_id: "KNOF".into(),
                    clean_days: 5,
                    days_checked: 5,
                    last_report,
                    forecast_through: None,
                    eligible: false,
                },
            ]
        );
        // The first day's reports alone judge one day.
        let one_day = access.eligibility(1, 24, NOW).await.unwrap();
        assert!(one_day.iter().all(|station| station.days_checked == 1));
        assert_eq!(one_day[0].clean_days, 1);

        // Reports read ahead judge every shorter history and window as the
        // files do.
        let reports = access.read_ahead_reports(NOW).await.unwrap();
        assert!(reports > 0);
        let read_later = NOW + Duration::minutes(20);
        for (days, window_hours) in [(5, 24), (3, 6), (1, 24), (7, 12)] {
            let files = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
                directory.path().to_string_lossy().into_owned(),
            )));
            assert_eq!(
                access
                    .eligibility(days, window_hours, read_later)
                    .await
                    .unwrap(),
                files
                    .eligibility(days, window_hours, read_later)
                    .await
                    .unwrap(),
                "{days} days, {window_hours} hours"
            );
        }
    }

    #[test]
    fn reports_read_ahead_serve_the_same_utc_day_for_a_while() {
        let timeline = Timeline {
            read_at: NOW,
            days: PRECOMPUTED_DAYS,
            reports: BTreeMap::new(),
            forecasts: BTreeMap::new(),
        };
        assert!(timeline.covers(3, NOW));
        assert!(timeline.covers(PRECOMPUTED_DAYS, NOW + Duration::minutes(30)));
        assert!(!timeline.covers(PRECOMPUTED_DAYS + 1, NOW));
        assert!(!timeline.covers(3, NOW - Duration::SECOND));
        assert!(!timeline.covers(3, NOW + TIMELINE_MAX_AGE));
        let late = Timeline {
            read_at: datetime!(2026-01-20 23:30 UTC),
            ..timeline
        };
        assert!(!late.covers(3, datetime!(2026-01-21 00:10 UTC)));
    }

    /// Hourly reports over five days from `stations` stations, each
    /// missing the hours `missed` gives it on a day (0 to 4).
    fn network(stations: usize, missed: impl Fn(usize, i64) -> Vec<i64>) -> Timeline {
        let start = datetime!(2026-01-15 00:53 UTC);
        let reports = (0..stations)
            .map(|station| {
                let reports = (0..5 * 24)
                    .filter(|hour| !missed(station, hour / 24).contains(&(hour % 24)))
                    .map(|hour| Report {
                        time: start + Duration::hours(hour),
                        temperature: true,
                    })
                    .chain([Report {
                        time: NOW - Duration::minutes(10),
                        temperature: true,
                    }])
                    .collect();
                (format!("K{station:03}"), reports)
            })
            .collect::<BTreeMap<_, Vec<_>>>();
        let forecasts = reports
            .keys()
            .map(|station| (station.clone(), Some(NOW + Duration::days(2))))
            .collect();
        Timeline {
            read_at: NOW,
            days: 5,
            reports,
            forecasts,
        }
    }

    #[test]
    fn days_the_whole_network_missed_are_not_held_against_stations() {
        // Every station misses 10:53 and 11:53 on the second day; station
        // 1 also misses them on the third, station 2 on the third and
        // fourth.
        let missed = |station: usize, day: i64| match (station, day) {
            (_, 1) | (1 | 2, 2) | (2, 3) => vec![10, 11],
            _ => vec![],
        };
        let judged = judge(&network(OUTAGE_MIN_STATIONS, missed), 5, 24, NOW);
        assert_eq!(judged.len(), OUTAGE_MIN_STATIONS);
        assert!(judged.iter().all(|station| station.days_checked == 4));
        let summary: Vec<_> = judged[..3]
            .iter()
            .map(|station| (station.clean_days, station.eligible))
            .collect();
        assert_eq!(summary, [(4, true), (3, true), (2, false)]);
        assert!(judged[3..].iter().all(|station| station.eligible));

        // Too few stations to tell an outage: the day counts against all.
        let judged = judge(&network(OUTAGE_MIN_STATIONS - 1, missed), 5, 24, NOW);
        assert!(judged.iter().all(|station| station.days_checked == 5));
        let summary: Vec<_> = judged[..3]
            .iter()
            .map(|station| (station.clean_days, station.eligible))
            .collect();
        assert_eq!(summary, [(4, true), (3, false), (2, false)]);
    }

    /// Corrected publications replace old conflicts, but conflicts or rejected
    /// reports in the newest publication must still create coverage gaps.
    #[tokio::test]
    async fn eligibility_uses_newest_publication_without_hiding_its_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        for (name, newest) in [
            ("observations_2026-01-20T11:54:00Z.parquet", false),
            ("observations_2026-01-20T11:55:00Z.parquet", true),
        ] {
            let repeated = if newest {
                "station_id = 'KDUP' OR (station_id = 'KCON' AND hour(ts) = 3)"
            } else {
                "station_id = 'KFIX' AND hour(ts) = 3"
            };
            let quality = if newest {
                "CASE WHEN station_id = 'KBAD' AND hour(ts) = 3 THEN 'rejected' ELSE 'validated' END"
            } else {
                "'validated'"
            };
            write(
                &directory,
                name,
                &format!(
                    "SELECT station_id, strftime(ts, '%Y-%m-%dT%H:%M:%SZ') AS generated_at,
                            (10 + CASE WHEN station_id != 'KDUP' THEN n ELSE 0 END)::DOUBLE AS temperature_value,
                            'celsius' AS temperature_unit_code, 'METAR' AS metar_type,
                            'METAR ' || station_id || ' ' || strftime(ts, '%d%H%MZ') || ' 09008KT 10SM 10/08 RMK AO2' AS raw_text,
                            {quality} AS quality_status, 'metar-consistency-v1' AS validation_version,
                            NULL::VARCHAR AS quality_reason, NULL::VARCHAR AS quality_metrics
                     FROM (VALUES ('KFIX'), ('KCON'), ('KDUP'), ('KBAD')) AS stations(station_id),
                          generate_series(TIMESTAMP '2026-01-17 00:53:00', TIMESTAMP '2026-01-20 11:53:00', INTERVAL 1 HOUR) AS hours(ts),
                          range(2) AS copies(n)
                     WHERE n = 0 OR ({repeated})"
                ),
            );
        }
        write(
            &directory,
            "forecasts_2026-01-20T11:30:00Z.parquet",
            "SELECT station_id, '2026-01-20T12:00:00Z' AS begin_time,
                    '2026-01-22T12:00:00Z' AS end_time, '2026-01-20T11:00:00Z' AS generated_at,
                    30::BIGINT AS min_temp, 40::BIGINT AS max_temp, 'fahrenheit' AS temperature_unit_code
             FROM (VALUES ('KFIX'), ('KCON'), ('KDUP'), ('KBAD')) AS stations(station_id)",
        );
        let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
            directory.path().to_string_lossy().into_owned(),
        )));
        let stations = access.eligibility(3, 6, NOW).await.unwrap();
        let results: Vec<_> = stations
            .iter()
            .map(|station| {
                (
                    station.station_id.as_str(),
                    station.clean_days,
                    station.eligible,
                )
            })
            .collect();
        assert_eq!(
            results,
            vec![
                ("KBAD", 0, false),
                ("KCON", 0, false),
                ("KDUP", 3, true),
                ("KFIX", 3, true),
            ]
        );
    }

    /// How long a cold computation takes over a month of hourly reports
    /// from 800 stations, about 600,000 reports in daily files, and a
    /// forecast for each. Run with
    /// `cargo test -p oracle --lib eligibility_benchmark -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "benchmark"]
    async fn eligibility_benchmark() {
        let directory = tempfile::tempdir().unwrap();
        let stations = "SELECT 'K' || chr(65 + (n // 676)::INT) || chr(65 + (n // 26 % 26)::INT) || chr(65 + (n % 26)::INT) AS station_id
                        FROM range(800) AS ids(n)";
        for day in 0..31 {
            let date = datetime!(2025-12-20 00:00 UTC) + Duration::days(day);
            let next = date + Duration::DAY;
            write(
                &directory,
                &format!("observations_{}.parquet", next.format(&Rfc3339).unwrap()),
                &format!(
                    "SELECT station_id, strftime(ts, '%Y-%m-%dT%H:%M:%SZ') AS generated_at,
                            10.0::DOUBLE AS temperature_value, 'celsius' AS temperature_unit_code,
                            'METAR' AS metar_type, 'METAR ' || station_id || ' 09008KT 10SM 10/08 RMK AO2' AS raw_text,
                            'validated' AS quality_status, 'metar-consistency-v1' AS validation_version,
                            NULL::VARCHAR AS quality_reason, NULL::VARCHAR AS quality_metrics
                     FROM ({stations}),
                          generate_series(TIMESTAMP '{} 00:53:00', TIMESTAMP '{} 23:53:00', INTERVAL 1 HOUR) AS hours(ts)",
                    date.date(),
                    date.date()
                ),
            );
        }
        write(
            &directory,
            "forecasts_2026-01-20T00:45:00Z.parquet",
            &format!(
                "SELECT station_id, '2026-01-20T00:00:00Z' AS begin_time, '2026-01-22T00:00:00Z' AS end_time,
                        '2026-01-20T00:30:00Z' AS generated_at, 30::BIGINT AS min_temp, 40::BIGINT AS max_temp,
                        'fahrenheit' AS temperature_unit_code
                 FROM ({stations})"
            ),
        );
        let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
            directory.path().to_string_lossy().into_owned(),
        )));
        let started = std::time::Instant::now();
        let stations = access
            .eligibility(
                DEFAULT_DAYS,
                DEFAULT_WINDOW_HOURS,
                datetime!(2026-01-20 01:00 UTC),
            )
            .await
            .unwrap();
        println!(
            "judged {} stations, {} eligible, in {:?}",
            stations.len(),
            stations.iter().filter(|station| station.eligible).count(),
            started.elapsed()
        );
        assert_eq!(stations.len(), 800);
    }
}
