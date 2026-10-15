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
/// Windows at least this long tolerate sensor outages; see [`longest_gap`].
const LONG_WINDOW: Duration = Duration::hours(12);
const PHASE_CLOSURE_LIMIT: Duration = Duration::minutes(75);
/// The longest gap one missed hourly report leaves with the reports on both
/// sides of it present: what station selection tolerates in a long window.
const MISSED_REPORT_GAP: Duration = Duration::minutes(150);

/// The longest gap a long window tolerates, its end included: a sixth of
/// it, 4 hours of a day and 2 of half a day. Small automated stations lose
/// their wind or temperature for one to four hours in snow or freezing fog,
/// and hourly stations miss a report now and then. Over the stations
/// competitions used in October 2026, 99.6% of day windows had no longer
/// gap in either metric; the rest were outages lasting days.
fn longest_gap(window: Duration) -> Duration {
    window / 6
}

/// The most of a long window that gaps over [`MAX_REPORT_GAP`] may cover
/// together: a quarter of it, 6 hours of a day.
fn gaps_allowed(window: Duration) -> Duration {
    window / 4
}

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

/// Why the reports usable for one metric do not sample a window.
#[derive(Debug, PartialEq, Eq)]
enum Shortfall {
    /// No report inside the window.
    Empty,
    /// A gap over 90 minutes in a window too short to tolerate one.
    Gap(Duration),
    /// A gap, the end's included, longer than the window tolerates.
    LongGap { gap: Duration, limit: Duration },
    /// Gaps over 90 minutes that together cover more than the window tolerates.
    Outages { total: Duration, limit: Duration },
    /// The last report is further before the window end than it tolerates.
    Tail { gap: Duration, limit: Duration },
}

impl std::fmt::Display for Shortfall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "no report inside the window"),
            Self::Gap(gap) => write!(
                f,
                "a {}-minute gap, and windows under {} hours tolerate none",
                gap.whole_minutes(),
                LONG_WINDOW.whole_hours()
            ),
            Self::LongGap { gap, limit } => write!(
                f,
                "a {}-minute gap, longer than the {} minutes this window tolerates",
                gap.whole_minutes(),
                limit.whole_minutes()
            ),
            Self::Outages { total, limit } => write!(
                f,
                "gaps over 90 minutes cover {} minutes, more than the {} minutes this window tolerates",
                total.whole_minutes(),
                limit.whole_minutes()
            ),
            Self::Tail { gap, limit } => write!(
                f,
                "the last report is {} minutes before the window end, more than the {} minutes this window tolerates",
                gap.whole_minutes(),
                limit.whole_minutes()
            ),
        }
    }
}

/// How `times`, the reports usable for one metric, sample the window. A
/// shorter window than [`LONG_WINDOW`] tolerates no gap over
/// [`MAX_REPORT_GAP`], its end included. A longer one tolerates gaps of up to
/// [`longest_gap`], its end included, while those over [`MAX_REPORT_GAP`]
/// cover no more than [`gaps_allowed`]. A tolerated gap adds no reading: the
/// hours missed are simply not observed.
fn sampling<'a>(
    times: impl Iterator<Item = &'a Report>,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<(), Shortfall> {
    report_gaps(times.map(|report| report.time), start, end)
}

/// [`sampling`] over report instants alone.
fn report_gaps(
    times: impl Iterator<Item = OffsetDateTime>,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<(), Shortfall> {
    let window = end - start;
    let long = window >= LONG_WINDOW;
    let (longest, allowed) = (longest_gap(window), gaps_allowed(window));
    let mut missing = Duration::ZERO;
    let mut check = |gap: Duration, tail: bool| {
        if gap <= MAX_REPORT_GAP {
            return Ok(());
        }
        if !long {
            return Err(if tail {
                Shortfall::Tail {
                    gap,
                    limit: MAX_REPORT_GAP,
                }
            } else {
                Shortfall::Gap(gap)
            });
        }
        if gap > longest {
            return Err(if tail {
                Shortfall::Tail {
                    gap,
                    limit: longest,
                }
            } else {
                Shortfall::LongGap {
                    gap,
                    limit: longest,
                }
            });
        }
        missing += gap;
        if missing > allowed {
            return Err(Shortfall::Outages {
                total: missing,
                limit: allowed,
            });
        }
        Ok(())
    };
    let mut previous = start;
    let mut count = 0;
    for time in times.filter(|time| *time >= start && *time < end) {
        check(time - previous, false)?;
        previous = time;
        count += 1;
    }
    if count == 0 {
        return Err(Shortfall::Empty);
    }
    check(end - previous, true)
}

/// Whether `times`, the reports usable for one metric, sample the window.
fn sampling_complete<'a>(
    times: impl Iterator<Item = &'a Report>,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> bool {
    sampling(times, start, end).is_ok()
}

/// Whether reports at `times`, in order, sample the window by the stricter
/// rule station selection applies: no gap over [`MAX_REPORT_GAP`], but in a
/// window of at least [`LONG_WINDOW`] one of up to [`MISSED_REPORT_GAP`], one
/// missed hourly report, before its end. A station that met it recently is
/// offered for competitions, and settlement, which tolerates the outages
/// storms bring ([`sampling`]), settles every window it accepts.
pub(super) fn times_sample_window(
    times: impl IntoIterator<Item = OffsetDateTime>,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> bool {
    let tolerates_one = end - start >= LONG_WINDOW;
    let mut tolerated = false;
    let mut previous = start;
    let mut count = 0;
    for time in times
        .into_iter()
        .filter(|time| *time >= start && *time < end)
    {
        let gap = time - previous;
        if gap > MAX_REPORT_GAP {
            if !tolerates_one || gap > MISSED_REPORT_GAP || tolerated {
                return false;
            }
            tolerated = true;
        }
        previous = time;
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

/// What the settlement query found for a station over the window, before
/// coverage is judged: its highest and lowest usable temperature in the unit
/// of `unit_code`, and its strongest usable wind in knots.
#[derive(Debug, Default)]
struct Extremes {
    temp_high: Option<f64>,
    temp_low: Option<f64>,
    unit_code: String,
    wind_speed: Option<i64>,
}

impl Extremes {
    /// The value `pair` is judged on, in the pair's unit, as settlement
    /// decodes it.
    fn value(&self, pair: &super::Decisive) -> Option<f64> {
        let temperature = |value: f64| super::convert(value, &self.unit_code, &pair.unit);
        match pair.metric.as_str() {
            "temp_high" => self.temp_high.map(temperature),
            "temp_low" => self.temp_low.map(temperature),
            "wind_speed" => super::within(self.wind_speed, 0..=500).map(|value| value as f64),
            _ => None,
        }
    }
}

/// [`Extremes`] by station, from the settlement query's rows.
fn extremes(batches: &[RecordBatch]) -> Result<HashMap<String, Extremes>, Error> {
    let mut found = HashMap::new();
    for batch in batches {
        let stations = super::strings(batch, "station_id")?;
        let temp_high = super::doubles(batch, "temp_high")?;
        let temp_low = super::doubles(batch, "temp_low")?;
        let unit_code = super::strings(batch, "temperature_unit_code")?;
        let wind_speed = super::integers(batch, "wind_speed")?;
        for row in 0..batch.num_rows() {
            let Some(station) = super::text(stations, row) else {
                continue;
            };
            found.insert(
                station,
                Extremes {
                    temp_high: super::double(temp_high, row),
                    temp_low: super::double(temp_low, row),
                    unit_code: super::text(unit_code, row).unwrap_or_default(),
                    wind_speed: super::integer(wind_speed, row),
                },
            );
        }
    }
    Ok(found)
}

/// Whether a station's reports sample the event window, both all of them and
/// those with a usable temperature when the event scores one.
fn sampled(
    station: &str,
    reports: &[Report],
    requirement: &coverage::Requirement,
    extremes: Option<&Extremes>,
) -> Result<(), Error> {
    if let Err(shortfall) = sampling(reports.iter(), requirement.start, requirement.end) {
        return Err(Error::ObservationCoverage {
            stations: vec![station.into()],
            reason: format!(
                "usable reports leave a gap longer than the window tolerates, or no report inside the scoring window ({shortfall})"
            ),
        });
    }
    // An unverified report drops out of the metrics its problems affect
    // (the query removed those values); the rest must still sample the
    // window. Wind and humidity gaps leave their values missing in totals.
    let gaps: Vec<String> = ["temp_high", "temp_low"]
        .into_iter()
        .filter(|metric| requirement.scores(metric))
        .map(|metric| format!("{station}/{metric}"))
        .collect();
    if gaps.is_empty() {
        return Ok(());
    }
    if let Err(shortfall) = sampling(
        reports.iter().filter(|row| row.temperature),
        requirement.start,
        requirement.end,
    ) {
        // An outage cannot move a line the reports there are already
        // decide: every scored temperature must be decided.
        let decided = ["temp_high", "temp_low"]
            .into_iter()
            .filter(|metric| requirement.scores(metric))
            .all(|metric| {
                extremes.is_some_and(|found| {
                    requirement.decides(station, metric, |pair| found.value(pair))
                })
            });
        if decided {
            log::info!(
                "{station}: temperature reports leave a gap settlement does not tolerate ({shortfall}), but they already decide its lines"
            );
            return Ok(());
        }
        let dropped = reports
            .iter()
            .filter(|row| {
                !row.verified && row.time >= requirement.start && row.time < requirement.end
            })
            .count();
        return Err(Error::ObservationCoverage {
            stations: vec![station.into()],
            reason: format!(
                "usable reports for {} leave a gap longer than the window tolerates, or none inside the scoring window ({dropped} unverified reports left out; {shortfall})",
                gaps.join(", ")
            ),
        });
    }
    Ok(())
}

/// Whether every station's reports sample the event window. All of them are
/// checked, so a block names each station short of the window, not only the
/// first by id. A station without any report is named beside them; on its own
/// it is left, as before, to the readings check that also holds settlement.
fn every_station_sampled(
    by_station: &BTreeMap<String, Vec<Report>>,
    requirement: &coverage::Requirement,
    extremes: &HashMap<String, Extremes>,
) -> Result<(), Error> {
    let mut short = Vec::new();
    for (station, reports) in by_station {
        if let Err(error) = sampled(station, reports, requirement, extremes.get(station)) {
            short.push(error);
        }
    }
    if short.is_empty() {
        return Ok(());
    }
    let mut silent: Vec<&String> = requirement
        .stations
        .iter()
        .filter(|station| !by_station.contains_key(*station))
        .collect();
    silent.sort();
    silent.dedup();
    for station in silent {
        if let Err(error) = sampled(station, &[], requirement, None) {
            short.push(error);
        }
    }
    if short.len() == 1 {
        return Err(short.remove(0));
    }
    let mut stations = Vec::new();
    let mut reasons = Vec::new();
    for error in short {
        match error {
            Error::ObservationCoverage {
                stations: named,
                reason,
            } => {
                reasons.push(format!("{}: {reason}", named.join(", ")));
                stations.extend(named);
            }
            error => return Err(error),
        }
    }
    Err(Error::ObservationCoverage {
        stations,
        reason: reasons.join("; "),
    })
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
    let found = extremes(batches)?;
    every_station_sampled(&by_station, requirement, &found)?;
    let mut measured = HashMap::new();
    for (station, reports) in by_station {
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
                    let station = stations.value(row);
                    let value = super::integer(old, row);
                    let sampled = measured.get(station).is_some_and(complete);
                    // The strongest wind of the reports there are can
                    // already decide its line through an outage.
                    let decided = !sampled
                        && measured.contains_key(station)
                        && requirement.decides(station, name, |_| {
                            super::within(value, 0..=500).map(|value| value as f64)
                        });
                    if decided {
                        log::info!(
                            "{station}: wind reports leave a gap settlement does not tolerate, but already decide its line"
                        );
                    }
                    value.filter(|_| sampled || decided)
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

    /// Hourly routine reports from the window start for `hours`, without
    /// those at the minutes in `missed`.
    fn hourly(hours: i64, missed: &[i64]) -> Vec<Report> {
        (0..hours)
            .map(|hour| hour * 60)
            .filter(|minutes| !missed.contains(minutes))
            .map(|minutes| report(minutes, true, 0.0))
            .collect()
    }

    fn window(hours: i64) -> (OffsetDateTime, OffsetDateTime) {
        let start = datetime!(2026-01-17 00:00 UTC);
        (start, start + Duration::hours(hours))
    }

    #[test]
    fn a_long_window_tolerates_sensor_outages_of_a_few_hours() {
        let (start, end) = window(24);
        let reports = hourly(24, &[600]);
        assert_eq!(sampling(reports.iter(), start, end), Ok(()));
        // Two missed reports apart, and a three-hour outage, as KSAA's wind
        // and temperature left in snow on 2026-10-15.
        assert_eq!(
            sampling(hourly(24, &[600, 1020]).iter(), start, end),
            Ok(())
        );
        assert_eq!(sampling(hourly(24, &[540, 600]).iter(), start, end), Ok(()));
        // A gap of up to 4 hours of a day, but no longer.
        assert_eq!(
            sampling(hourly(24, &[540, 600, 660]).iter(), start, end),
            Ok(())
        );
        assert_eq!(
            sampling(hourly(24, &[540, 600, 660, 720]).iter(), start, end),
            Err(Shortfall::LongGap {
                gap: Duration::minutes(300),
                limit: Duration::minutes(240)
            })
        );
        // Gaps over 90 minutes may cover 6 hours of a day together.
        assert_eq!(
            sampling(hourly(24, &[180, 240, 600, 660]).iter(), start, end),
            Ok(())
        );
        assert_eq!(
            sampling(
                hourly(24, &[180, 240, 600, 660, 1020, 1080]).iter(),
                start,
                end
            ),
            Err(Shortfall::Outages {
                total: Duration::minutes(540),
                limit: Duration::minutes(360)
            })
        );
        // Half a day tolerates half as much.
        let (half_start, half_end) = window(12);
        assert_eq!(
            sampling(hourly(12, &[300]).iter(), half_start, half_end),
            Ok(())
        );
        assert_eq!(
            sampling(hourly(12, &[240, 300]).iter(), half_start, half_end),
            Err(Shortfall::LongGap {
                gap: Duration::minutes(180),
                limit: Duration::minutes(120)
            })
        );
        // The tolerated gap leaves no value for the missed hour: the
        // accumulation chain breaks there rather than being filled in.
        // The closing report at the window end makes the chain complete
        // without the gap.
        assert_eq!(totals(&hourly(25, &[]), start, end).rain, Some(0.0));
        let mut humid = hourly(25, &[600]);
        humid[9].humidity = Some(97);
        let result = totals(&humid, start, end);
        assert!(result.wind_complete && result.direction_complete);
        assert_eq!(result.humidity, Some(97));
        assert_eq!(result.rain, None);
    }

    /// Station selection keeps the stricter rule: every window it accepts
    /// settles, and it turns down the stations whose sensors just failed.
    #[test]
    fn station_selection_is_stricter_than_settlement() {
        for (hours, missed, selected) in [
            (24, &[][..], true),
            (24, &[600][..], true),
            (24, &[600, 1020][..], false),
            (24, &[540, 600][..], false),
            (6, &[120][..], false),
            (6, &[300][..], false),
            (24, &[1380][..], false),
        ] {
            let (start, end) = window(hours);
            let reports = hourly(hours, missed);
            let accepted =
                times_sample_window(reports.iter().map(|report| report.time), start, end);
            assert_eq!(accepted, selected, "{hours} hours without {missed:?}");
            if accepted {
                assert_eq!(sampling(reports.iter(), start, end), Ok(()));
            }
        }
        let (start, end) = window(24);
        assert!(!times_sample_window([], start, end));
    }

    #[test]
    fn a_short_window_tolerates_no_gap_and_a_long_one_no_long_outage_at_its_end() {
        let (start, end) = window(6);
        assert_eq!(sampling(hourly(6, &[]).iter(), start, end), Ok(()));
        assert_eq!(
            sampling(hourly(6, &[120]).iter(), start, end),
            Err(Shortfall::Gap(Duration::minutes(120)))
        );
        assert_eq!(
            sampling(hourly(6, &[300]).iter(), start, end),
            Err(Shortfall::Tail {
                gap: Duration::minutes(120),
                limit: Duration::minutes(90)
            })
        );
        // A sensor that stops before the end of a long window, as KMYP's
        // wind did in freezing fog on 2026-10-15, is an outage like another.
        let (start, end) = window(24);
        assert_eq!(sampling(hourly(24, &[1380]).iter(), start, end), Ok(()));
        assert_eq!(
            sampling(hourly(24, &[1260, 1320, 1380]).iter(), start, end),
            Ok(())
        );
        assert_eq!(
            sampling(hourly(24, &[1200, 1260, 1320, 1380]).iter(), start, end),
            Err(Shortfall::Tail {
                gap: Duration::minutes(300),
                limit: Duration::minutes(240)
            })
        );
        assert_eq!(
            sampling(std::iter::empty(), start, end),
            Err(Shortfall::Empty)
        );
    }

    #[test]
    fn unverified_temperatures_in_a_long_window_are_tolerated_like_missing_reports() {
        let (start, end) = window(24);
        let requirement = coverage::Requirement {
            start,
            end,
            stations: vec!["KJFK".into()],
            collected_after: end,
            groups: super::super::quality_groups(&[]),
            metrics: vec!["temp_high".into()],
            decisive: vec![],
        };
        let mut reports = hourly(24, &[]);
        reports[10].verified = false;
        reports[10].temperature = false;
        assert!(sampled("KJFK", &reports, &requirement, None).is_ok());
        reports[16].verified = false;
        reports[16].temperature = false;
        assert!(sampled("KJFK", &reports, &requirement, None).is_ok());
        for report in &mut reports[15..19] {
            report.verified = false;
            report.temperature = false;
        }
        let Err(Error::ObservationCoverage { stations, reason }) =
            sampled("KJFK", &reports, &requirement, None)
        else {
            panic!("five hours without a usable temperature hold settlement");
        };
        assert_eq!(stations, ["KJFK"]);
        assert!(reason.contains("KJFK/temp_high"), "{reason}");
        assert!(reason.contains("5 unverified reports left out"), "{reason}");
        assert!(
            reason.contains("a 300-minute gap, longer than the 240 minutes"),
            "{reason}"
        );
        let mut short = requirement.clone();
        short.end = start + Duration::hours(6);
        assert!(sampled("KJFK", &reports[..6], &short, None).is_ok());
        reports[3].temperature = false;
        assert!(sampled("KJFK", &reports[..6], &short, None).is_err());
    }

    #[test]
    fn a_block_names_every_station_short_of_the_window() {
        let (start, end) = window(24);
        let requirement = coverage::Requirement {
            start,
            end,
            stations: vec!["KBFM".into(), "KECP".into(), "KGZH".into(), "KEGI".into()],
            collected_after: end,
            groups: super::super::quality_groups(&[]),
            metrics: vec!["temp_high".into()],
            decisive: vec![],
        };
        // KBFM misses four reports in a row, KGZH's last report is seven
        // hours before the end, KECP reports throughout and KEGI sends nothing.
        let by_station = BTreeMap::from([
            ("KBFM".to_string(), hourly(24, &[600, 660, 720, 780])),
            ("KECP".to_string(), hourly(24, &[])),
            ("KGZH".to_string(), hourly(18, &[])),
        ]);
        let Err(Error::ObservationCoverage { stations, reason }) =
            every_station_sampled(&by_station, &requirement, &HashMap::new())
        else {
            panic!("stations short of the window hold settlement");
        };
        assert_eq!(stations, ["KBFM", "KGZH", "KEGI"]);
        assert!(reason.starts_with("KBFM: "), "{reason}");
        assert!(reason.contains("a 300-minute gap"), "{reason}");
        assert!(
            reason.contains("; KGZH: ")
                && reason.contains("the last report is 420 minutes before the window end"),
            "{reason}"
        );
        assert!(
            reason.contains("; KEGI: ") && reason.contains("no report inside the window"),
            "{reason}"
        );
        assert!(!reason.contains("KECP"), "{reason}");

        // One station short of the window is blocked as before.
        let mut pair = requirement.clone();
        pair.stations = vec!["KBFM".into(), "KECP".into()];
        let one = BTreeMap::from([
            ("KBFM".to_string(), hourly(24, &[600, 660, 720, 780])),
            ("KECP".to_string(), hourly(24, &[])),
        ]);
        let Err(Error::ObservationCoverage { stations, reason }) =
            every_station_sampled(&one, &pair, &HashMap::new())
        else {
            panic!("KBFM is short of the window");
        };
        let Err(Error::ObservationCoverage { reason: alone, .. }) =
            sampled("KBFM", &one["KBFM"], &pair, None)
        else {
            panic!("KBFM is short of the window");
        };
        assert_eq!(stations, ["KBFM"]);
        assert_eq!(reason, alone);

        // A station without any report is otherwise left to the readings check.
        let quiet = BTreeMap::from([("KECP".to_string(), hourly(24, &[]))]);
        assert!(every_station_sampled(&quiet, &requirement, &HashMap::new()).is_ok());
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
            rows[0].temp_high,
            Some(14.0),
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
            partial[0].temp_high,
            Some(14.0),
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
                rows[0].temp_high,
                Some(14.0),
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
