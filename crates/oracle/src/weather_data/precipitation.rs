//! METAR totals are accumulations since the previous routine report.
//! SPECI reports share that anchor; adding their prefixes double counts rain.

use super::{Error, coverage};
use duckdb::{
    Connection,
    arrow::array::{ArrayRef, Float64Array, RecordBatch, StringArray},
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

const MAX_REPORT_GAP: Duration = Duration::minutes(90);
const PHASE_CLOSURE_LIMIT: Duration = Duration::minutes(75);

#[derive(Debug)]
struct Report {
    time: OffsetDateTime,
    routine: bool,
    verified: bool,
    raw: String,
    amount: Option<f64>,
    wind: bool,
    direction: bool,
    temperature: bool,
    humidity: Option<i64>,
}

#[derive(Default, Debug)]
struct Totals {
    rain: Option<f64>,
    snow: Option<f64>,
    ice: Option<f64>,
    wind_complete: bool,
    direction_complete: bool,
    humidity: Option<i64>,
}

#[derive(Clone, Copy)]
struct Interval {
    start: OffsetDateTime,
    end: OffsetDateTime,
    amount: f64,
}

/// Sum an exact end-to-start chain. Prefer the longest interval from each
/// anchor, so an hourly METAR replaces every overlapping SPECI prefix.
fn chain(mut intervals: Vec<Interval>, start: OffsetDateTime, end: OffsetDateTime) -> Option<f64> {
    intervals.retain(|row| row.start >= start && row.end <= end && row.start < row.end);
    intervals.sort_by_key(|row| (row.start, std::cmp::Reverse(row.end)));
    let mut cursor = start;
    let mut total = 0.0;
    for row in intervals {
        if row.start != cursor {
            continue;
        }
        cursor = row.end;
        total += row.amount;
        if cursor == end {
            return total.is_finite().then_some(total);
        }
    }
    None
}

/// Whether `times`, the reports usable for one metric, sample the window.
fn sampling_complete<'a>(
    times: impl Iterator<Item = &'a Report>,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> bool {
    let mut previous = start;
    let mut count = 0;
    for report in times.filter(|row| row.time >= start && row.time < end) {
        if report.time - previous > MAX_REPORT_GAP {
            return false;
        }
        previous = report.time;
        count += 1;
    }
    count > 0 && end - previous <= MAX_REPORT_GAP
}

/// Solid or freezing precipitation makes liquid-only rainfall ambiguous.
/// Inspect raw weather groups, including beginning/ending remarks, rather
/// than inferring phase from the current temperature.
fn frozen_weather(raw: &str) -> bool {
    let mut remarks = false;
    raw.split_whitespace().skip(2).any(|token| {
        if token == "RMK" {
            remarks = true;
            return false;
        }
        let token = token.trim_start_matches(['+', '-']);
        if remarks {
            return [
                "SNB", "SNE", "PLB", "PLE", "FZRAB", "FZRAE", "FZDZB", "FZDZE", "GRB", "GRE",
                "GSB", "GSE", "SGB", "SGE", "ICB", "ICE", "UPB", "UPE", "SNINCR", "PWINO",
            ]
            .iter()
            .any(|part| token.contains(part));
        }
        token.bytes().all(|byte| byte.is_ascii_uppercase())
            && token.len() <= 8
            && ["SN", "SG", "PL", "IC", "GR", "GS", "UP", "FZRA", "FZDZ"]
                .iter()
                .any(|code| token.contains(code))
    })
}

fn totals(reports: &[Report], start: OffsetDateTime, end: OffsetDateTime) -> Totals {
    let mut rain = Vec::new();
    let mut snow = Vec::new();
    let mut ice = Vec::new();
    let mut anchor: Option<usize> = None;
    let mut anchor_hourly = false;
    for (index, report) in reports.iter().enumerate() {
        if let Some(previous) = anchor {
            let before = &reports[previous];
            let duration = report.time - before.time;
            let hourly = duration >= Duration::minutes(45) && duration <= Duration::minutes(75);
            let anchored = if report.routine {
                hourly
            } else {
                anchor_hourly
            };
            if before.verified
                && report.verified
                && !report
                    .raw
                    .split_whitespace()
                    .any(|token| token.trim_end_matches('=') == "P0000")
                && anchored
                && report.time > before.time
                && duration <= Duration::minutes(75)
                && let Some(amount) = report
                    .amount
                    .filter(|value| value.is_finite() && *value >= 0.0)
            {
                let span = &reports[previous..=index];
                let cumulative: Vec<_> = reports[previous + 1..=index]
                    .iter()
                    .filter_map(|row| row.amount)
                    .collect();
                if cumulative.windows(2).any(|pair| pair[1] + 1e-6 < pair[0]) {
                    if report.routine {
                        anchor = Some(index);
                        anchor_hourly = hourly;
                    }
                    continue;
                }
                let known_phase = span.iter().all(|row| row.verified)
                    && !span.iter().any(|row| frozen_weather(&row.raw));
                let period = Interval {
                    start: before.time,
                    end: report.time,
                    amount,
                };
                if known_phase || amount == 0.0 {
                    rain.push(period);
                    snow.push(Interval {
                        amount: 0.0,
                        ..period
                    });
                    ice.push(Interval {
                        amount: 0.0,
                        ..period
                    });
                }
                // A numerical 10:1 liquid-to-snow estimate is not a measured
                // snowfall total. Positive mixed/snow intervals stay missing.
            }
        }
        if report.routine {
            anchor_hourly = anchor.is_some_and(|previous| {
                let gap = report.time - reports[previous].time;
                gap >= Duration::minutes(45) && gap <= Duration::minutes(75)
            });
            anchor = Some(index);
        }
    }
    Totals {
        rain: chain(rain, start, end),
        snow: chain(snow, start, end),
        ice: chain(ice, start, end),
        wind_complete: sampling_complete(reports.iter().filter(|row| row.wind), start, end),
        direction_complete: sampling_complete(
            reports.iter().filter(|row| row.direction),
            start,
            end,
        ),
        humidity: sampling_complete(
            reports.iter().filter(|row| row.humidity.is_some()),
            start,
            end,
        )
        .then(|| {
            reports
                .iter()
                .filter(|row| row.verified && row.time >= start && row.time < end)
                .filter_map(|row| row.humidity)
                .max()
        })
        .flatten(),
    }
}

fn phase_window(
    reports: &[Report],
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let anchor = reports
        .iter()
        .rev()
        .find(|row| row.routine && row.time <= start)?;
    let closing = reports.iter().find(|row| row.routine && row.time >= end)?;
    if !anchor.verified
        || !closing.verified
        || start - anchor.time > MAX_REPORT_GAP
        || closing.time - end > PHASE_CLOSURE_LIMIT
    {
        return None;
    }
    Some((anchor.time, closing.time))
}

fn phase_clear(reports: &[Report], (start, end): (OffsetDateTime, OffsetDateTime)) -> bool {
    sampling_complete(reports.iter().filter(|row| row.verified), start, end)
        && reports
            .iter()
            .filter(|row| row.time >= start && row.time <= end)
            .all(|row| row.verified && !frozen_weather(&row.raw))
}

fn apply_fixed_hours(value: &mut Totals, fixed_total: Option<f64>, phase_known: bool) {
    // RR7 measures liquid equivalent; report coverage and phase still
    // come from the independently validated METAR/SPECI history.
    let total = fixed_total.filter(|amount| *amount == 0.0 || phase_known);
    value.rain = total;
    // A measured zero liquid total can establish zero measurable rain. It
    // cannot establish zero snowfall depth while precipitation phase is unknown.
    value.snow = total.filter(|_| phase_known).map(|_| 0.0);
    value.ice = total.filter(|_| phase_known).map(|_| 0.0);
}

pub(super) fn apply(
    connection: &Connection,
    files: &[String],
    rows_sql: &str,
    requirement: &coverage::Requirement,
    batches: &[RecordBatch],
) -> Result<Vec<RecordBatch>, Error> {
    let mut statement = connection.prepare(rows_sql)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<f64>>(4)?,
            row.get::<_, bool>(5)?,
            row.get::<_, bool>(6)?,
            row.get::<_, bool>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, bool>(9)?,
        ))
    })?;
    let mut by_station = BTreeMap::<String, Vec<Report>>::new();
    for row in rows {
        let (
            station,
            timestamp,
            kind,
            raw,
            amount,
            verified,
            wind,
            direction,
            humidity,
            temperature,
        ) = row?;
        let time = OffsetDateTime::parse(&timestamp, &Rfc3339)?;
        by_station.entry(station).or_default().push(Report {
            time,
            routine: kind.as_deref() == Some("METAR"),
            verified,
            raw: raw.unwrap_or_default(),
            amount,
            wind,
            direction,
            temperature,
            humidity: humidity.filter(|value| (0..=100).contains(value)),
        });
    }
    let fixed_hours = super::shef::Evidence::read(connection, files)?;
    let mut measured = HashMap::new();
    for (station, reports) in by_station {
        if !sampling_complete(reports.iter(), requirement.start, requirement.end) {
            return Err(Error::ObservationCoverage {
                stations: vec![station],
                reason: "usable reports have a gap longer than 90 minutes, or no report inside the scoring window".into(),
            });
        }
        // An unverified report drops out of the metrics its problems affect
        // (the query removed those values); the rest must still sample the
        // window. Wind and humidity gaps leave their values missing below.
        let gaps: Vec<String> = ["temp_high", "temp_low"]
            .into_iter()
            .filter(|metric| requirement.scores(metric))
            .map(|metric| format!("{station}/{metric}"))
            .collect();
        if !gaps.is_empty()
            && !sampling_complete(
                reports.iter().filter(|row| row.temperature),
                requirement.start,
                requirement.end,
            )
        {
            let dropped = reports
                .iter()
                .filter(|row| {
                    !row.verified && row.time >= requirement.start && row.time < requirement.end
                })
                .count();
            return Err(Error::ObservationCoverage {
                stations: vec![station],
                reason: format!(
                    "usable reports for {} have a gap longer than 90 minutes, or none inside the scoring window ({dropped} unverified reports left out)",
                    gaps.join(", ")
                ),
            });
        }
        let mut value = totals(&reports, requirement.start, requirement.end);
        let phase_known =
            phase_window(&reports, requirement.start, requirement.end).is_some_and(|phase| {
                let mut closure = requirement.clone();
                closure.start = phase.0;
                closure.end = phase.1;
                closure.stations = vec![station.clone()];
                phase_clear(&reports, phase) && closure.verify(connection, files).is_ok()
            });
        if !phase_known {
            value.rain = None;
            value.snow = None;
            value.ice = None;
        }
        if let Some(fixed_total) = fixed_hours.total(&station, requirement) {
            apply_fixed_hours(&mut value, fixed_total, phase_known);
        }
        // Reports were verified for the event's metrics only: a rain gauge
        // outage cannot hold a temperature event, nor can its totals stand.
        if !requirement.groups.contains(&"precipitation") {
            value.rain = None;
            value.snow = None;
            value.ice = None;
        }
        measured.insert(station, value);
    }
    let mut output = Vec::new();
    for batch in batches {
        let station_index = batch
            .schema()
            .index_of("station_id")
            .map_err(|_| Error::QualityUnavailable)?;
        let stations = batch
            .column(station_index)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or(Error::QualityUnavailable)?;
        let mut columns = batch.columns().to_vec();
        for (name, field) in [
            (
                "rain_amt",
                (|value: &Totals| value.rain) as fn(&Totals) -> Option<f64>,
            ),
            ("snow_amt", |value: &Totals| value.snow),
            ("ice_amt", |value: &Totals| value.ice),
        ] {
            let index = batch
                .schema()
                .index_of(name)
                .map_err(|_| Error::QualityUnavailable)?;
            let values: Float64Array = (0..batch.num_rows())
                .map(|row| measured.get(stations.value(row)).and_then(field))
                .collect();
            columns[index] = Arc::new(values) as ArrayRef;
        }
        // Missing samples of optional metrics cannot silently become an
        // aggregate of only the surviving hours.
        for (name, complete) in [
            (
                "wind_speed",
                (|value: &Totals| value.wind_complete) as fn(&Totals) -> bool,
            ),
            ("wind_direction", |value: &Totals| value.direction_complete),
        ] {
            let index = batch
                .schema()
                .index_of(name)
                .map_err(|_| Error::QualityUnavailable)?;
            let old = super::integers(batch, name)?;
            let values: duckdb::arrow::array::Int64Array = (0..batch.num_rows())
                .map(|row| {
                    measured
                        .get(stations.value(row))
                        .filter(|value| complete(value))
                        .and_then(|_| super::integer(old, row))
                })
                .collect();
            columns[index] = Arc::new(values) as ArrayRef;
        }
        let index = batch
            .schema()
            .index_of("humidity")
            .map_err(|_| Error::QualityUnavailable)?;
        let values: duckdb::arrow::array::Int64Array = (0..batch.num_rows())
            .map(|row| {
                measured
                    .get(stations.value(row))
                    .and_then(|value| value.humidity)
            })
            .collect();
        columns[index] = Arc::new(values) as ArrayRef;
        output.push(
            RecordBatch::try_new(batch.schema(), columns).map_err(|_| Error::QualityUnavailable)?,
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn report(minutes: i64, routine: bool, amount: f64) -> Report {
        Report {
            time: datetime!(2026-01-17 00:00 UTC) + Duration::minutes(minutes),
            routine,
            amount: Some(amount),
            verified: true,
            raw: "METAR KORD 170000Z 18005KT 10SM RA 10/08 RMK AO2".into(),
            wind: true,
            direction: true,
            temperature: true,
            humidity: Some(80),
        }
    }

    #[test]
    fn complete_rain_window_counts_hourly_totals_once_and_includes_endpoint() {
        let reports = [
            report(-60, true, 0.0),
            report(0, true, 0.0),
            report(20, false, 0.1),
            report(40, false, 0.2),
            report(60, true, 0.3),
            report(120, true, 0.2),
        ];
        let result = totals(&reports, reports[1].time, reports[5].time);
        assert_eq!(result.rain, Some(0.5));
        assert_eq!(result.snow, Some(0.0));
        assert_eq!(
            totals(&reports, reports[1].time, reports[3].time).rain,
            Some(0.2)
        );
    }

    #[test]
    fn cut_intervals_gaps_and_missing_anchor_stay_unknown() {
        let reports = [
            report(0, true, 0.0),
            report(60, true, 0.2),
            report(180, true, 0.3),
        ];
        assert_eq!(
            totals(
                &reports,
                reports[0].time + Duration::minutes(10),
                reports[1].time
            )
            .rain,
            None
        );
        assert_eq!(
            totals(&reports, reports[0].time, reports[2].time).rain,
            None
        );
        assert_eq!(
            totals(&reports[1..], reports[0].time, reports[1].time).rain,
            None
        );
    }

    #[test]
    fn snowfall_is_not_invented_from_liquid_equivalent() {
        let mut reports = [report(0, true, 0.0), report(60, true, 0.2)];
        reports[1].raw = "METAR KORD 170100Z 18005KT 1SM SN 00/M02 RMK AO2 P0020".into();
        let result = totals(&reports, reports[0].time, reports[1].time);
        assert_eq!(result.rain, None);
        assert_eq!(result.snow, None);
    }

    #[test]
    fn mixed_unknown_and_trace_precipitation_require_window_review() {
        for raw in [
            "METAR KORD 170100Z 18005KT 1SM UP 00/M02 RMK AO2 P0020",
            "METAR KORD 170100Z 18005KT 1SM RA 00/M02 RMK AO2 RAB01SNB30SNE40 P0020",
            "METAR KORD 170100Z 18005KT 1SM SN 00/M02 RMK AO2 P0000",
        ] {
            let mut reports = [report(0, true, 0.0), report(60, true, 0.2)];
            reports[1].raw = raw.into();
            if raw.contains("P0000") {
                reports[1].amount = Some(0.0);
            }
            let result = totals(&reports, reports[0].time, reports[1].time);
            assert_eq!(result.rain, None, "{raw}");
            assert_eq!(result.snow, None, "{raw}");
        }
    }

    #[test]
    fn station_outage_cannot_pass_as_continuous_sampling() {
        let reports = [report(0, true, 0.0), report(60, true, 0.0)];
        assert!(sampling_complete(
            reports.iter(),
            reports[0].time,
            reports[1].time + Duration::minutes(30)
        ));
        assert!(!sampling_complete(
            reports.iter(),
            reports[0].time,
            reports[1].time + Duration::hours(3)
        ));
    }

    #[test]
    fn frequent_routine_reports_and_decreasing_prefixes_do_not_invent_hourly_totals() {
        let frequent = [
            report(0, true, 0.0),
            report(20, true, 0.1),
            report(40, true, 0.2),
            report(60, true, 0.3),
        ];
        assert_eq!(
            totals(&frequent, frequent[0].time, frequent[3].time).rain,
            None
        );
        let contradiction = [
            report(0, true, 0.0),
            report(20, false, 0.4),
            report(60, true, 0.3),
        ];
        assert_eq!(
            totals(&contradiction, contradiction[0].time, contradiction[2].time).rain,
            None
        );
    }

    #[test]
    fn fixed_hour_liquid_totals_still_require_usable_phase_evidence() {
        let mut reports = [
            report(-9, true, 0.0),
            report(51, true, 0.1),
            report(111, true, 0.14),
        ];
        let start = reports[0].time + Duration::minutes(9);
        let end = start + Duration::HOUR;
        let phase_known = |reports: &[Report]| {
            phase_window(reports, start, end).is_some_and(|window| phase_clear(reports, window))
        };
        let mut result = totals(&reports, start, end);
        assert_eq!(
            result.rain, None,
            "ordinary METAR minutes cannot establish a whole-hour total"
        );
        apply_fixed_hours(&mut result, Some(0.14), phase_known(&reports));
        assert_eq!(result.rain, Some(0.14));
        assert_eq!(result.snow, Some(0.0));
        reports[1].raw = "METAR KORD 170051Z 18005KT 1SM UP 00/M02 RMK AO2 P0010".into();
        apply_fixed_hours(&mut result, Some(0.14), phase_known(&reports));
        assert_eq!(result.rain, None);
        assert_eq!(
            result.snow, None,
            "liquid equivalent is not measured snowfall"
        );
        apply_fixed_hours(&mut result, Some(0.0), phase_known(&reports));
        assert_eq!(
            result.rain,
            Some(0.0),
            "an explicit zero liquid measurement establishes zero measurable rain"
        );
        assert_eq!(
            result.snow, None,
            "unknown phase cannot establish a snowfall depth"
        );
        assert_eq!(result.ice, None);
        apply_fixed_hours(&mut result, None, phase_known(&reports));
        assert_eq!(result.rain, None);
    }

    #[test]
    fn post_window_snow_remarks_missing_and_late_closure_hold_positive_rain() {
        let start = datetime!(2026-01-17 05:00 UTC);
        let end = start + Duration::HOUR;
        let mut reports = [
            report(291, true, 0.0),
            report(351, true, 0.1),
            report(411, true, 0.14),
        ];
        let phase_known = |reports: &[Report]| {
            phase_window(reports, start, end).is_some_and(|window| phase_clear(reports, window))
        };
        assert!(phase_known(&reports));
        assert!(
            !phase_known(&reports[..2]),
            "an in-window report does not close precipitation phase"
        );
        reports[2].raw =
            "METAR KORD 170651Z 18005KT 10SM RA 10/08 RMK AO2 SNB0555E0605 P0014".into();
        assert!(
            !phase_known(&reports),
            "the closing report establishes snow inside the event"
        );
        reports[2].raw =
            "METAR KORD 170651Z 18005KT 10SM RA 10/08 RMK AO2 SNB0605E0610 P0014".into();
        assert!(
            !phase_known(&reports),
            "do not guess phase from late remarks even if onset appears after the event"
        );
        reports[2].raw = reports[1].raw.clone();
        reports[2].verified = false;
        assert!(!phase_known(&reports));
        reports[2].verified = true;
        reports[2].time = end + Duration::minutes(76);
        assert!(
            !phase_known(&reports),
            "unbounded future reports cannot close an event"
        );
        reports[2].time = end + Duration::minutes(75);
        assert!(
            phase_known(&reports),
            "the documented 75-minute closure boundary is inclusive"
        );
    }

    #[test]
    fn humidity_uses_the_observed_maximum_and_requires_reporting_coverage() {
        let mut reports = [
            report(0, true, 0.0),
            report(60, true, 0.0),
            report(120, true, 0.0),
        ];
        reports[0].humidity = Some(53);
        reports[1].humidity = Some(94);
        reports[2].humidity = Some(99);
        let result = totals(&reports, reports[0].time, reports[2].time);
        assert_eq!(
            result.humidity,
            Some(94),
            "match forecast maximum and exclude ending point"
        );
        reports[1].humidity = None;
        assert_eq!(
            totals(&reports, reports[0].time, reports[2].time).humidity,
            None
        );
    }

    #[tokio::test]
    async fn parquet_receipts_and_report_intervals_authorize_nonzero_rain_without_endpoint_temperature()
     {
        use super::super::{
            ObservationRequest, TemperatureUnit, WeatherAccess, WeatherData, open_connection,
        };
        let directory = tempfile::tempdir().unwrap();
        let day = directory.path().join("2026-01-17");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join("observations_2026-01-17T20:30:00Z.parquet");
        let receipt = serde_json::json!({
            "version":"awc-history-v1", "interval":"closed", "batches":[{
                "station_ids":["KORD"], "window_start":"2026-01-17T17:00:00Z",
                "window_end":"2026-01-17T20:29:59Z", "requested_at":"2026-01-17T20:30:00Z",
                "completed_at":"2026-01-17T20:31:00Z", "status":"complete",
                "source_url":"https://aviationweather.gov/api/data/metar?ids=KORD&format=xml",
                "source_sha256":"a".repeat(64),"response_status":200,"response_count":6,"report_count":6,"error":null
            }]
        }).to_string().replace('\'', "''");
        let connection = open_connection().unwrap();
        connection.execute_batch(&format!(r#"
            COPY (
                SELECT 'KORD' AS station_id, generated_at, temperature_value::DOUBLE AS temperature_value,
                    'celsius' AS temperature_unit_code, 'METAR' AS unused,
                    metar_type, raw_text, precip_in::DOUBLE AS precip_in, 'inches' AS precip_unit_code,
                    'RA' AS wx_string, 8::BIGINT AS wind_speed, 'knots' AS wind_speed_unit_code,
                    90::BIGINT AS wind_direction, 'degrees true' AS wind_direction_unit_code,
                    (temperature_value-2)::DOUBLE AS dewpoint_value, 'celsius' AS dewpoint_unit_code,
                    'validated' AS quality_status, 'metar-consistency-v1' AS validation_version
                FROM (VALUES
                    ('2026-01-17T17:00:00Z',10,'METAR',0.0,'METAR KORD 171700Z 09008KT 10SM RA 10/08 RMK AO2'),
                    ('2026-01-17T18:00:00Z',11,'METAR',0.0,'METAR KORD 171800Z 09008KT 10SM RA 11/09 RMK AO2'),
                    ('2026-01-17T18:20:00Z',12,'SPECI',0.1,'SPECI KORD 171820Z 09008KT 10SM RA 12/10 RMK AO2 P0010'),
                    ('2026-01-17T18:40:00Z',13,'SPECI',0.2,'SPECI KORD 171840Z 09008KT 10SM RA 13/11 RMK AO2 P0020'),
                    ('2026-01-17T19:00:00Z',14,'METAR',0.3,'METAR KORD 171900Z 09008KT 10SM RA 14/12 RMK AO2 P0030'),
                    ('2026-01-17T20:00:00Z',24,'METAR',0.2,'METAR KORD 172000Z 09008KT 10SM RA 24/22 RMK AO2 P0020')
                ) AS reports(generated_at,temperature_value,metar_type,precip_in,raw_text)
            ) TO '{}' (FORMAT PARQUET, KV_METADATA {{observation_coverage:'{receipt}'}})
        "#, path.display())).unwrap();
        let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
            directory.path().to_string_lossy().into_owned(),
        )));
        let mut request = ObservationRequest {
            start: Some(datetime!(2026-01-17 18:00 UTC)),
            end: Some(datetime!(2026-01-17 20:00 UTC)),
            station_ids: "KORD".into(),
            temperature_unit: TemperatureUnit::Celsius,
        };
        let rows = access
            .settlement_observations(
                &request,
                vec!["KORD".into()],
                datetime!(2026-01-17 20:15 UTC),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].temp_high, 14.0,
            "endpoint report is accumulation evidence, not an inside-window temperature"
        );
        assert_eq!(
            rows[0].rain_amt,
            Some(0.5),
            "SPECI prefixes must not add another0.3"
        );
        assert_eq!(rows[0].snow_amt, Some(0.0));
        request.start = Some(datetime!(2026-01-17 18:10 UTC));
        let partial = access
            .settlement_observations(
                &request,
                vec!["KORD".into()],
                datetime!(2026-01-17 20:15 UTC),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            partial[0].rain_amt, None,
            "cutting an accumulation does not authorize a guessed total"
        );
        assert_eq!(
            partial[0].temp_high, 14.0,
            "an incompatible rain window does not erase temperature"
        );
    }

    #[tokio::test]
    async fn post_window_phase_closure_requires_history_without_changing_point_extrema() {
        use super::super::{
            ObservationRequest, TemperatureUnit, WeatherAccess, WeatherData, open_connection,
        };
        for (name, closing, remark, receipt_end, expected) in [
            (
                "complete phase history",
                true,
                "",
                "2026-01-17T20:29:59Z",
                Some(0.5),
            ),
            (
                "post-event snow overlaps the event",
                true,
                "SNB1925E1940",
                "2026-01-17T20:29:59Z",
                None,
            ),
            (
                "no routine closing report",
                false,
                "",
                "2026-01-17T20:29:59Z",
                None,
            ),
            (
                "collection stops before closure",
                true,
                "",
                "2026-01-17T19:40:00Z",
                None,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let day = directory.path().join("2026-01-17");
            std::fs::create_dir_all(&day).unwrap();
            let path = day.join("observations_2026-01-17T20:30:00Z.parquet");
            let receipt = serde_json::json!({
                "version":"awc-history-v1", "interval":"closed", "batches":[{
                    "station_ids":["KORD"], "window_start":"2026-01-17T17:00:00Z",
                    "window_end":receipt_end, "requested_at":"2026-01-17T20:30:00Z",
                    "completed_at":"2026-01-17T20:31:00Z", "status":"complete",
                    "source_url":"https://aviationweather.gov/api/data/metar?ids=KORD&format=xml",
                    "source_sha256":"a".repeat(64),"response_status":200,"response_count":5,"report_count":5,"error":null
                }]
            }).to_string().replace('\'',"''");
            let closing_row = if closing {
                format!(
                    ",('2026-01-17T20:00:00Z',24,'METAR',0.3,'METAR KORD 172000Z 09008KT 10SM RA 24/22 RMK AO2 {remark} P0030')"
                )
            } else {
                String::new()
            };
            let connection = open_connection().unwrap();
            connection.execute_batch(&format!(r#"
                COPY (
                    SELECT 'KORD' AS station_id, generated_at, temperature_value::DOUBLE AS temperature_value,
                        'celsius' AS temperature_unit_code, metar_type, raw_text,
                        precip_in::DOUBLE AS precip_in, 'inches' AS precip_unit_code,
                        'RA' AS wx_string, 8::BIGINT AS wind_speed, 'knots' AS wind_speed_unit_code,
                        90::BIGINT AS wind_direction, 'degrees true' AS wind_direction_unit_code,
                        (temperature_value-2)::DOUBLE AS dewpoint_value, 'celsius' AS dewpoint_unit_code,
                        'validated' AS quality_status, 'metar-consistency-v1' AS validation_version
                    FROM (VALUES
                        ('2026-01-17T17:00:00Z',10,'METAR',0.0,'METAR KORD 171700Z 09008KT 10SM RA 10/08 RMK AO2'),
                        ('2026-01-17T18:00:00Z',11,'METAR',0.0,'METAR KORD 171800Z 09008KT 10SM RA 11/09 RMK AO2'),
                        ('2026-01-17T19:00:00Z',14,'METAR',0.3,'METAR KORD 171900Z 09008KT 10SM RA 14/12 RMK AO2 P0030'),
                        ('2026-01-17T19:30:00Z',15,'SPECI',0.2,'SPECI KORD 171930Z 09008KT 10SM RA 15/13 RMK AO2 P0020')
                        {closing_row}
                    ) AS reports(generated_at,temperature_value,metar_type,precip_in,raw_text)
                ) TO '{}' (FORMAT PARQUET, KV_METADATA {{observation_coverage:'{receipt}'}})
            "#,path.display())).unwrap();
            let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
                directory.path().to_string_lossy().into_owned(),
            )));
            let request = ObservationRequest {
                start: Some(datetime!(2026-01-17 18:00 UTC)),
                end: Some(datetime!(2026-01-17 19:30 UTC)),
                station_ids: "KORD".into(),
                temperature_unit: TemperatureUnit::Celsius,
            };
            let rows = access
                .settlement_observations(
                    &request,
                    vec!["KORD".into()],
                    datetime!(2026-01-17 19:35 UTC),
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(rows.len(), 1, "{name}");
            assert_eq!(
                rows[0].temp_high, 14.0,
                "{name}: post-window evidence must not alter point extrema"
            );
            assert_eq!(
                rows[0].wind_speed,
                Some(8),
                "{name}: a rain hold must not erase valid wind"
            );
            assert_eq!(rows[0].rain_amt, expected, "{name}");
            assert_eq!(rows[0].snow_amt, expected.map(|_| 0.0), "{name}");
        }
    }
}
