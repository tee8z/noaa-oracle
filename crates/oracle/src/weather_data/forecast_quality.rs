//! Settlement reads native source periods from one eligible publication.
//! Display caches may aggregate days; they cannot establish a settlement window.

use super::*;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
};

const VERSION: &str = "ndfd-native-v1";
const SOURCE_PREFIX: &str =
    "https://graphical.weather.gov/xml/sample_products/browser_interface/ndfdXMLclient.php?";
const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const METRICS: [&str; 7] = [
    "temp_high",
    "temp_low",
    "wind_speed",
    "wind_direction",
    "rain_amt",
    "snow_amt",
    "humidity",
];

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct NativeValue {
    layout: String,
    start: String,
    end: Option<String>,
    index: usize,
    value: String,
    units: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Row {
    station_id: String,
    begin: Option<i64>,
    end: Option<i64>,
    generated: Option<i64>,
    published: Option<i64>,
    source: String,
    values: BTreeMap<String, Option<f64>>,
    forecast_interval_version: Option<String>,
    interval_kind: Option<String>,
    source_url: Option<String>,
    source_received_at: Option<String>,
    source_xml_sha256: Option<String>,
    source_location: Option<String>,
    source_layouts: Option<String>,
    quality_status: Option<String>,
    quality_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct SourceDocument {
    url: String,
    received_at: String,
    sha256: String,
    encoding: String,
    content: String,
}

type Documents = BTreeMap<(String, String), SourceDocument>;
type Check<T> = Result<T, &'static str>;

fn micros(time: OffsetDateTime) -> i64 {
    (time.unix_timestamp_nanos() / 1_000) as i64
}

fn parsed(value: &str) -> Option<i64> {
    OffsetDateTime::parse(value, &Rfc3339).ok().map(micros)
}

fn source_name(path: &str) -> String {
    path.rsplit('/')
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("/")
}

fn row_query(
    files: &[String],
    filter: &str,
    issued: (OffsetDateTime, OffsetDateTime),
) -> Result<String, Error> {
    let rows = source_forecast_rows_sql(files, filter);
    Ok(format!(
        r#"SELECT to_json(struct_pack(
        station_id := station_id, "begin" := epoch_us(begin_ts), "end" := epoch_us(end_ts),
        generated := epoch_us(generated_ts), published := epoch_us(published_ts), source := source,
        "values" := struct_pack(min_temp := min_temp, max_temp := max_temp,
            wind_speed := wind_speed, wind_direction := wind_direction,
            relative_humidity_max := relative_humidity_max, relative_humidity_min := relative_humidity_min,
            liquid_precipitation_amt := liquid_precipitation_amt, snow_amt := snow_amt,
            snow_ratio := snow_ratio, ice_amt := ice_amt),
        forecast_interval_version := forecast_interval_version, interval_kind := interval_kind,
        source_url := source_url, source_received_at := source_received_at,
        source_xml_sha256 := source_xml_sha256, source_location := source_location,
        source_layouts := source_layouts, quality_status := quality_status, quality_reason := quality_reason
    ))::VARCHAR AS forecast_json FROM ({rows})
    WHERE generated_ts IS NULL OR (generated_ts >= '{}'::TIMESTAMPTZ AND generated_ts <= '{}'::TIMESTAMPTZ)"#,
        issued.0.format(&Rfc3339)?,
        issued.1.format(&Rfc3339)?
    ))
}

fn decode_rows(batches: &[RecordBatch]) -> Result<Vec<Row>, Error> {
    let mut rows = vec![];
    for batch in batches {
        let json = strings(batch, "forecast_json")?;
        for index in 0..batch.num_rows() {
            let row = serde_json::from_str(json.value(index)).map_err(|error| {
                Error::ForecastQuality {
                    reason: format!("native forecast row cannot be decoded: {error}"),
                }
            })?;
            rows.push(row);
        }
    }
    Ok(rows)
}

fn document_valid(document: &SourceDocument, key: &str) -> bool {
    if document.encoding != "gzip+base64"
        || document.sha256 != key
        || key.len() != 64
        || !key.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !document.url.starts_with(SOURCE_PREFIX)
        || parsed(&document.received_at).is_none()
        || document.content.len() > MAX_SOURCE_BYTES as usize * 2
    {
        return false;
    }
    let Ok(compressed) = base64::engine::general_purpose::STANDARD.decode(&document.content) else {
        return false;
    };
    let mut raw = vec![];
    if flate2::read::GzDecoder::new(compressed.as_slice())
        .take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut raw)
        .is_err()
        || raw.is_empty()
        || raw.len() as u64 > MAX_SOURCE_BYTES
    {
        return false;
    }
    let hash = Sha256::digest(&raw)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    hash == key
}

fn retained_documents(
    connection: &Connection,
    files: &[String],
    needed: &BTreeSet<(String, String)>,
) -> Result<Documents, Error> {
    let mut documents = Documents::new();
    let mut conflicts = BTreeSet::new();
    let sql = format!(
        "SELECT file_name, decode(key), decode(value) FROM parquet_kv_metadata([{}]) WHERE starts_with(decode(key), 'noaa_forecast_source:')",
        sql_string_list(files)
    );
    let mut query = connection.prepare(&sql)?;
    let entries = query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for entry in entries {
        let (file, key, value) = entry?;
        let hash = key.trim_start_matches("noaa_forecast_source:");
        let entry = (source_name(&file), hash.to_owned());
        if !needed.contains(&entry) {
            continue;
        }
        let Ok(document) = serde_json::from_str::<SourceDocument>(&value) else {
            conflicts.insert(entry);
            continue;
        };
        if !document_valid(&document, hash)
            || documents
                .get(&entry)
                .is_some_and(|previous| previous != &document)
        {
            conflicts.insert(entry);
        } else {
            documents.insert(entry, document);
        }
    }
    for conflict in conflicts {
        documents.remove(&conflict);
    }
    Ok(documents)
}

impl WeatherAccess {
    pub(super) async fn native_forecast_assessment(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<ForecastAssessment>, Error> {
        let (Some(start), Some(end)) = (req.start, req.end) else {
            return Err(Error::ForecastQuality {
                reason: "settlement needs an explicit forecast window".into(),
            });
        };
        if start >= end || station_ids.is_empty() {
            return Err(Error::ForecastQuality {
                reason: "settlement needs stations and a nonempty forecast window".into(),
            });
        }
        let filter = station_filter(&station_ids)?;
        let now = OffsetDateTime::now_utc();
        let eligible_at = micros(start.min(now));
        let mut issued = forecast_generated_window(req, now);
        issued.1 = issued.1.min(start - Duration::nanoseconds(1));
        let parameters = forecast_file_params(issued, now);
        let files = if issued.0 > issued.1 || empty_publication_window(&parameters) {
            vec![]
        } else {
            self.file_access
                .build_file_paths(self.file_access.grab_file_names(parameters).await?)
        };
        if files.is_empty() {
            return Ok(evaluate_selected(
                &[],
                &Documents::new(),
                &station_ids,
                micros(start),
                micros(end),
            ));
        }
        let sql = row_query(&files, &filter, issued)?;
        self.query_with_connection(sql, move |connection, batches| {
            let rows = decode_rows(batches)?;
            let selected = select_rows(&rows, eligible_at);
            let needed: BTreeSet<_> = selected.iter().map(|row| row.source.as_str()).collect();
            let selected_files = files
                .iter()
                .filter(|file| needed.contains(source_name(file).as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let needed_documents = selected
                .iter()
                .filter_map(|row| {
                    row.source_xml_sha256
                        .as_ref()
                        .map(|hash| (row.source.clone(), hash.clone()))
                })
                .collect();
            let documents = if selected_files.is_empty() {
                Documents::new()
            } else {
                retained_documents(connection, &selected_files, &needed_documents)?
            };
            Ok(evaluate_selected(
                &selected,
                &documents,
                &station_ids,
                micros(start),
                micros(end),
            ))
        })
        .await
    }
}

/// Publication eligibility is decided before newest-issue selection: a later
/// refetch must not hide the copy demonstrably received before the event.
fn select_rows(rows: &[Row], cutoff: i64) -> Vec<&Row> {
    let eligible = |row: &&Row| {
        row.generated.is_none_or(|generated| generated < cutoff)
            && match row.source_received_at.as_deref().and_then(parsed) {
                Some(received) => received <= cutoff,
                // Missing metadata in a pre-event file remains a candidate,
                // so a malformed latest publication cannot resurrect old data.
                None => row.published.is_some_and(|published| published <= cutoff),
            }
    };
    let mut latest = BTreeMap::<&str, (Option<i64>, Option<i64>)>::new();
    let mut publications = BTreeMap::<&str, Option<i64>>::new();
    for row in rows.iter().filter(eligible) {
        let order = (row.generated, row.published);
        publications
            .entry(row.station_id.as_str())
            .and_modify(|current| *current = (*current).max(row.published))
            .or_insert(row.published);
        latest
            .entry(row.station_id.as_str())
            .and_modify(|current| *current = (*current).max(order))
            .or_insert(order);
    }
    let missing_issue: BTreeSet<_> = rows
        .iter()
        .filter(eligible)
        .filter(|row| {
            row.generated.is_none()
                && publications.get(row.station_id.as_str()) == Some(&row.published)
        })
        .map(|row| row.station_id.as_str())
        .collect();
    rows.iter()
        .filter(eligible)
        .filter(|row| {
            if missing_issue.contains(row.station_id.as_str()) {
                row.generated.is_none()
                    && publications.get(row.station_id.as_str()) == Some(&row.published)
            } else {
                latest.get(row.station_id.as_str()) == Some(&(row.generated, row.published))
            }
        })
        .collect()
}

#[cfg(test)]
fn evaluate(
    rows: &[Row],
    documents: &Documents,
    stations: &[String],
    start: i64,
    end: i64,
) -> Vec<SettlementValue> {
    evaluate_selected(&select_rows(rows, start), documents, stations, start, end)
        .into_iter()
        .map(|value| SettlementValue {
            station_id: value.station_id,
            metric: value.metric,
            value: value.value,
        })
        .collect()
}

fn evaluate_selected(
    rows: &[&Row],
    documents: &Documents,
    stations: &[String],
    start: i64,
    end: i64,
) -> Vec<ForecastAssessment> {
    let mut output = vec![];
    for station in stations.iter().collect::<BTreeSet<_>>() {
        let station_rows: Vec<_> = rows
            .iter()
            .copied()
            .filter(|row| &row.station_id == station)
            .collect();
        let assessed = Station::new(&station_rows, documents, start);
        for metric in METRICS {
            let value = assessed
                .as_ref()
                .map_err(|reason| *reason)
                .and_then(|station| station.value(metric, start, end));
            output.push(ForecastAssessment {
                station_id: station.clone(),
                metric: metric.into(),
                value: value.ok(),
                reason: value.err().map(str::to_owned),
                native_intervals: assessed
                    .as_ref()
                    .map(|station| station.native_intervals(metric))
                    .unwrap_or_default(),
            });
        }
    }
    output
}

struct Station<'a> {
    rows: Vec<(&'a Row, BTreeMap<String, NativeValue>)>,
}

#[derive(Clone, Debug, PartialEq)]
struct Sample {
    start: i64,
    end: i64,
    value: f64,
}

impl<'a> Station<'a> {
    fn native_intervals(&self, metric: &str) -> Vec<ForecastNativeInterval> {
        let fields: &[&str] = match metric {
            "temp_high" => &["max_temp"],
            "temp_low" => &["min_temp"],
            "humidity" => &["relative_humidity_max"],
            "wind_speed" => &["wind_speed"],
            "wind_direction" => &["wind_speed", "wind_direction"],
            "snow_amt" => &["snow_amt"],
            "rain_amt" => &[
                "liquid_precipitation_amt",
                "snow_amt",
                "ice_amt",
                "snow_ratio",
            ],
            _ => &[],
        };
        let mut intervals = BTreeMap::new();
        for (row, layouts) in &self.rows {
            for metric in fields {
                let Some(native) = layouts.get(*metric) else {
                    continue;
                };
                let Some(start) = parsed(&native.start) else {
                    continue;
                };
                let end = match native.end.as_deref() {
                    Some(end) => match parsed(end) {
                        Some(end) if end > start => Some(end),
                        _ => continue,
                    },
                    None => None,
                };
                let instant = matches!(*metric, "wind_speed" | "wind_direction");
                if row.begin != Some(start)
                    || row.end != Some(end.unwrap_or(start))
                    || native.layout.is_empty()
                    || instant != end.is_none()
                    || row.interval_kind.as_deref()
                        != Some(if instant { "instant" } else { "period" })
                {
                    continue;
                }
                // Canonical UTC boundaries make equal instants with different
                // source offsets sort and deduplicate the same way.
                let timestamp = |value| {
                    OffsetDateTime::from_unix_timestamp_nanos(i128::from(value) * 1_000)
                        .ok()
                        .and_then(|time| time.format(&Rfc3339).ok())
                };
                let Some(start_text) = timestamp(start) else {
                    continue;
                };
                let end_text = end.and_then(timestamp);
                intervals.insert(
                    ((*metric).to_owned(), start, end),
                    ForecastNativeInterval {
                        metric: (*metric).into(),
                        start: start_text,
                        end: end_text,
                    },
                );
            }
        }
        intervals.into_values().collect()
    }

    fn new(rows: &[&'a Row], documents: &Documents, cutoff: i64) -> Check<Self> {
        if rows.is_empty() {
            return Err("no forecast publication was received before the event");
        }
        let identities: BTreeSet<_> = rows
            .iter()
            .map(|row| (&row.source, &row.source_xml_sha256, &row.source_location))
            .collect();
        if identities.len() != 1 {
            return Err("latest forecast publication has conflicting source identities");
        }
        let mut verified = vec![];
        for row in rows {
            if row.forecast_interval_version.as_deref() != Some(VERSION) {
                return Err("forecast lacks native interval provenance");
            }
            let received = row
                .source_received_at
                .as_deref()
                .and_then(parsed)
                .ok_or("forecast receipt time is missing")?;
            if received > cutoff
                || row
                    .generated
                    .is_none_or(|generated| generated > received + 600_000_000)
            {
                return Err("forecast issue or receipt time is not eligible");
            }
            if row.source_location.as_deref().is_none_or(str::is_empty) {
                return Err("forecast source location is missing");
            }
            let hash = row
                .source_xml_sha256
                .as_ref()
                .ok_or("forecast source hash is missing")?;
            let document = documents
                .get(&(row.source.clone(), hash.clone()))
                .ok_or("retained forecast source does not verify against its hash")?;
            if row.source_url.as_deref() != Some(document.url.as_str())
                || row.source_received_at.as_deref() != Some(document.received_at.as_str())
            {
                return Err("forecast source metadata differs from the retained document");
            }
            let layouts = serde_json::from_str::<BTreeMap<String, NativeValue>>(
                row.source_layouts
                    .as_deref()
                    .ok_or("native metric provenance is missing")?,
            )
            .map_err(|_| "native metric provenance is malformed")?;
            if layouts.is_empty() {
                return Err("native metric provenance is empty");
            }
            verified.push((*row, layouts));
        }
        Ok(Self { rows: verified })
    }

    fn samples(&self, metric: &str, start: i64, end: i64, instant: bool) -> Check<Vec<Sample>> {
        let mut samples = BTreeMap::<(i64, i64), Sample>::new();
        for (row, layouts) in &self.rows {
            let Some(native) = layouts.get(metric) else {
                if row.values.get(metric).copied().flatten().is_some() {
                    return Err("forecast value has no native source interval");
                }
                continue;
            };
            let begin = row.begin.ok_or("forecast interval start is missing")?;
            let finish = row.end.ok_or("forecast interval end is missing")?;
            if begin > finish {
                return Err("forecast interval is reversed");
            }
            // A period belongs to the window that holds its midpoint. Back to
            // back windows then share none, and any 24-hour window holds one
            // daytime high and one overnight low, whatever its start or the
            // station's time zone.
            if if begin == finish {
                begin < start || begin >= end
            } else {
                let middle = begin + (finish - begin) / 2;
                middle < start || middle >= end
            } {
                continue;
            }
            if row.quality_status.as_deref() != Some("validated")
                || row
                    .quality_reason
                    .as_deref()
                    .is_some_and(|reason| !reason.trim().is_empty())
            {
                return Err("native forecast row failed source validation");
            }
            if parsed(&native.start) != Some(begin)
                || native.end.as_deref().and_then(parsed).unwrap_or(begin) != finish
                || native.layout.is_empty()
                || (instant && native.end.is_some())
                || (!instant && native.end.is_none())
                || (instant && begin != finish)
                || (!instant && begin >= finish)
                || row.interval_kind.as_deref() != Some(if instant { "instant" } else { "period" })
            {
                return Err("metric interval differs from its native source layout");
            }
            let value = row
                .values
                .get(metric)
                .copied()
                .flatten()
                .ok_or("native forecast value is missing")?;
            let raw = native
                .value
                .trim()
                .parse::<f64>()
                .map_err(|_| "native source value is missing or malformed")?;
            let normalized = normalize(metric, &native.units, raw)?;
            if !value.is_finite() || (value - normalized).abs() > 1e-8 {
                return Err("forecast value differs from its retained source value");
            }
            let sample = Sample {
                start: begin,
                end: finish,
                value,
            };
            if samples
                .get(&(begin, finish))
                .is_some_and(|previous| previous != &sample)
            {
                return Err("latest publication contains conflicting metric values");
            }
            samples.insert((begin, finish), sample);
        }
        let samples: Vec<_> = samples.into_values().collect();
        if samples.is_empty() {
            return Err("no complete native source values cover this event metric");
        }
        for pair in samples.windows(2) {
            if pair[0].end > pair[1].start {
                return Err("native source periods overlap");
            }
        }
        Ok(samples)
    }

    fn wind_coverage(&self, start: i64, end: i64) -> Check<()> {
        let mut times = BTreeSet::new();
        for (row, layouts) in &self.rows {
            let Some(native) = layouts.get("wind_speed") else {
                continue;
            };
            let time = row.begin.ok_or("wind grid has no timestamp")?;
            if row.end != Some(time) || parsed(&native.start) != Some(time) || native.end.is_some()
            {
                return Err("wind grid differs from native instant metadata");
            }
            times.insert(time);
        }
        // NDFD starts a series at the next whole hour it holds, up to an hour
        // after the fetch, whatever the request's `begin`. A publication taken
        // just before an event can therefore start its winds shortly after the
        // event does; that hour's peak still comes from the forecast's own
        // hourly values, so accept one hourly step at the leading edge.
        const LEADING_SLACK: i64 = 3_600_000_000;
        let first = times
            .range(..=start)
            .next_back()
            .or_else(|| times.range(start..=start + LEADING_SLACK).next())
            .copied()
            .ok_or("wind source horizon begins after the event")?;
        let last = times
            .range(end..)
            .next()
            .copied()
            .ok_or("wind source horizon ends before the event")?;
        let selected = times.range(first..=last).copied().collect::<Vec<_>>();
        // The supported NDFD time-series grids step from hourly to three- and
        // six-hourly values. A larger gap cannot establish a full baseline.
        if selected
            .windows(2)
            .any(|pair| pair[1] - pair[0] > 6 * 3_600_000_000)
        {
            return Err("wind grid has a gap longer than six hours");
        }
        Ok(())
    }

    fn value(&self, metric: &str, start: i64, end: i64) -> Check<f64> {
        match metric {
            "temp_high" | "temp_low" | "humidity" => {
                let source = match metric {
                    "temp_high" => "max_temp",
                    "temp_low" => "min_temp",
                    _ => "relative_humidity_max",
                };
                let samples = self.samples(source, start, end, false)?;
                // One such period comes a day after the last (an hour either
                // way across a daylight saving change). A whole missing day
                // must not become a partial event baseline: no gap between
                // midpoints, or from the window's edges to them, may leave
                // room for another.
                const DAY: i64 = 86_400_000_000;
                const SLACK: i64 = 2 * 3_600_000_000;
                let middle = |sample: &Sample| sample.start + (sample.end - sample.start) / 2;
                let first = middle(&samples[0]);
                let last = middle(samples.last().unwrap());
                if first - start >= DAY + SLACK
                    || end - last > DAY + SLACK
                    || samples
                        .windows(2)
                        .any(|pair| middle(&pair[1]) - middle(&pair[0]) > DAY + SLACK)
                {
                    return Err("daily native extrema do not span the event window");
                }
                samples
                    .iter()
                    .map(|sample| sample.value)
                    .reduce(if metric == "temp_low" {
                        f64::min
                    } else {
                        f64::max
                    })
                    .ok_or("native extrema are missing")
            }
            "wind_speed" | "wind_direction" => {
                self.wind_coverage(start, end)?;
                let winds = self.samples("wind_speed", start, end, true)?;
                let peak = winds
                    .iter()
                    .max_by(|a, b| a.value.total_cmp(&b.value).then(a.start.cmp(&b.start)))
                    .unwrap();
                if metric == "wind_speed" {
                    return Ok(peak.value);
                }
                self.samples("wind_direction", start, end, true)?
                    .into_iter()
                    .find(|sample| sample.start == peak.start)
                    .map(|sample| sample.value)
                    .ok_or("direction at the peak wind is missing")
            }
            "snow_amt" => sum_chain(&self.samples("snow_amt", start, end, false)?, start, end),
            "rain_amt" => {
                let qpf = sum_chain(
                    &self.samples("liquid_precipitation_amt", start, end, false)?,
                    start,
                    end,
                )?;
                let snow = self.samples("snow_amt", start, end, false)?;
                sum_chain(&snow, start, end)?;
                let ice = sum_chain(&self.samples("ice_amt", start, end, false)?, start, end)?;
                if ice > 0.0 {
                    return Err("ice accretion has no verified liquid-equivalent conversion");
                }
                let mut snow_liquid = 0.0;
                if snow.iter().any(|sample| sample.value > 0.0) {
                    let ratios = self.samples("snow_ratio", start, end, false)?;
                    for sample in snow.iter().filter(|sample| sample.value > 0.0) {
                        let ratio = ratios
                            .iter()
                            .find(|ratio| {
                                ratio.start == sample.start
                                    && ratio.end == sample.end
                                    && ratio.value > 0.0
                            })
                            .ok_or("nonzero snow lacks a ratio for its exact source period")?;
                        snow_liquid += sample.value / ratio.value;
                    }
                }
                let rain = qpf - snow_liquid - ice;
                if !rain.is_finite() || rain < 0.0 {
                    Err("precipitation components are inconsistent")
                } else {
                    Ok(rain)
                }
            }
            _ => Err("unknown settlement metric"),
        }
    }
}

#[cfg(test)]
#[path = "forecast_quality_tests.rs"]
mod tests;

fn normalize(metric: &str, units: &str, value: f64) -> Check<f64> {
    if !value.is_finite() {
        return Err("source value is not finite");
    }
    let (value, minimum, maximum, integer) = match (metric, units.to_ascii_lowercase().as_str()) {
        ("min_temp" | "max_temp", "fahrenheit") => (value, -148.0, 140.0, true),
        ("min_temp" | "max_temp", "celsius" | "celcius") => {
            if value.fract() != 0.0 {
                return Err("source temperature is not a whole degree");
            }
            (value * 9.0 / 5.0 + 32.0, -148.0, 140.0, false)
        }
        ("wind_speed", "knots") => (value, 0.0, 250.0, true),
        ("wind_direction", "degrees true") => (value, 0.0, 360.0, true),
        ("relative_humidity_max" | "relative_humidity_min", "percent") => (value, 0.0, 100.0, true),
        ("snow_ratio", "percent") => (value, 0.0, 1000.0, false),
        ("liquid_precipitation_amt" | "snow_amt" | "ice_amt", "inches") => {
            (value, 0.0, f64::MAX, false)
        }
        _ => return Err("native source units are unsupported"),
    };
    if value < minimum || value > maximum || (integer && value.fract() != 0.0) {
        Err("source value is outside its accepted range")
    } else {
        Ok(value)
    }
}

/// Sums the precipitation periods whose midpoints fall in the window. They
/// must follow one another without a gap, and none may be missing at either
/// end: a period before the first, or after the last, of the same length
/// would have its midpoint in the window too.
fn sum_chain(samples: &[Sample], start: i64, end: i64) -> Check<f64> {
    const GAP: &str = "precipitation periods do not cover the event window";
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return Err(GAP);
    };
    if 2 * (first.start - start) >= first.end - first.start
        || 2 * (end - last.end) > last.end - last.start
    {
        return Err(GAP);
    }
    let mut through = first.start;
    let mut total = 0.0;
    for sample in samples {
        if sample.start != through || sample.end <= sample.start {
            return Err(GAP);
        }
        through = sample.end;
        total += sample.value;
    }
    if total.is_finite() {
        Ok(total)
    } else {
        Err(GAP)
    }
}
