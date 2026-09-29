use anyhow::{Error, anyhow};
use parquet::file::metadata::KeyValue;
use parquet::file::writer::SerializedFileWriter;
use parquet::record::RecordWriter;
use parquet::{
    basic::{LogicalType, Repetition, Type as PhysicalType},
    schema::types::Type,
};
use parquet_derive::ParquetRecordWriter;
use serde::Serialize;
use sha2::{Digest, Sha256};
use slog::{Logger, info, warn};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use time::{OffsetDateTime, format_description::well_known::Rfc3339, macros::format_description};

use super::history::{
    ArchivedResponse, HISTORY_SOURCE, HistoryCollection, HistoryConfig, HistoryQuery,
    ObservationCoverage, collect_history,
};
use crate::parquet_file::{self, PartialFile};
use crate::{CityWeather, Metar, Units, XmlFetcher};
#[cfg(test)]
use crate::{ObservationData, parse_xml};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
pub struct CurrentWeather {
    pub raw_text: String,
    pub metar_type: Option<String>,
    pub quality_status: String,
    pub quality_reason: Option<String>,
    /// Comma-separated [`MetricGroup`]s that the quality problems affect.
    pub quality_metrics: Option<String>,
    pub station_id: String,
    pub latitude: f64,
    pub longitude: f64,
    pub generated_at: OffsetDateTime,
    pub temperature_value: Option<f64>,
    pub temperature_unit_code: String,
    pub wind_direction: Option<i64>,
    pub wind_direction_unit_code: String,
    pub wind_speed: Option<i64>,
    pub wind_speed_unit_code: String,
    pub dewpoint_value: Option<f64>,
    pub dewpoint_unit_code: String,
    pub precip_in: Option<f64>,
    pub precip_unit_code: String,
    pub wx_string: String,
}

impl TryFrom<Metar> for CurrentWeather {
    type Error = anyhow::Error;
    fn try_from(val: Metar) -> Result<Self, Self::Error> {
        // Preserve malformed optional fields as rejected evidence, never as valid nulls.
        let mut problems = Problems::default();
        let temperature_value = checked_number(
            val.temp_c.as_deref(),
            "temperature",
            TEMPERATURE,
            &mut problems,
        );
        let dewpoint_value = checked_number(
            val.dewpoint_c.as_deref(),
            "dewpoint",
            DEWPOINT,
            &mut problems,
        );
        let wind_speed = checked_integer(
            val.wind_speed_kt.as_deref(),
            "wind_speed",
            WIND,
            &mut problems,
        );
        let wind_direction = if val.wind_dir_degrees.as_deref() == Some("VRB") {
            None
        } else {
            checked_integer(
                val.wind_dir_degrees.as_deref(),
                "wind_direction",
                WIND,
                &mut problems,
            )
        };
        if let Err(reason) =
            super::wind_validation::validate_wind(&val.raw_text, wind_speed, wind_direction)
        {
            problems.push(WIND, reason);
        }
        let raw_type = val
            .raw_text
            .split_whitespace()
            .next()
            .filter(|value| matches!(*value, "METAR" | "SPECI"));
        if val.metar_type.as_deref().is_some_and(|kind| {
            !matches!(kind, "METAR" | "SPECI") || raw_type.is_some_and(|raw| raw != kind)
        }) {
            problems.push(ALL, "decoded metar_type conflicts with raw report type");
        }
        let metar_type = val
            .metar_type
            .clone()
            .or_else(|| raw_type.map(str::to_owned));
        let decoded_precip = checked_number(
            val.precip_in.as_deref(),
            "precip_in",
            PRECIPITATION,
            &mut problems,
        );
        // An hourly P group is not mandatory in SPECI. Its absence cannot
        // establish zero accumulation, even when a decoder supplies zero.
        let special_without_precip = metar_type.as_deref() == Some("SPECI")
            && matches!(raw_precipitation(&val.raw_text), Ok(None));
        if special_without_precip && decoded_precip.is_some_and(|value| value != 0.0) {
            problems.push(
                PRECIPITATION,
                "decoded SPECI precipitation has no raw hourly P group",
            );
        }
        if temperature_value.is_none() {
            problems.push(TEMPERATURE, "missing usable decoded temperature");
        }
        if wind_speed.is_some_and(|v| v < 0) {
            problems.push(WIND, "negative wind speed");
        }
        if wind_direction.is_some_and(|v| !(0..=360).contains(&v)) {
            problems.push(WIND, "wind direction outside 0..360 degrees");
        }
        if decoded_precip.is_some_and(|v| v < 0.0) {
            problems.push(PRECIPITATION, "negative precipitation");
        }
        if let Err(error) = validate_precipitation(&val.raw_text, decoded_precip) {
            problems.push(PRECIPITATION, error.to_string());
        }
        if has_remark(&val.raw_text, "PNO") {
            problems.push(
                PRECIPITATION,
                "rain gauge outage (PNO); precipitation cannot be verified",
            );
        }
        if has_remark(&val.raw_text, "PWINO") {
            problems.push(
                PRESENT_WEATHER,
                "present weather sensor outage (PWINO); weather classification cannot be verified",
            );
        }
        if let Err(error) = validate_temperatures(&val.raw_text, temperature_value, dewpoint_value)
        {
            problems.push(TEMPERATURES, error.to_string());
        }
        let latitude = required_coordinate(val.latitude.as_deref(), "latitude", -90.0, 90.0)?;
        let longitude = required_coordinate(val.longitude.as_deref(), "longitude", -180.0, 180.0)?;
        let generated_at = OffsetDateTime::parse(
            val.observation_time
                .as_deref()
                .ok_or_else(|| anyhow!("report has no observation_time"))?,
            &Rfc3339,
        )
        .map_err(|e| anyhow!("error parsing observation_time: {e}"))?
        .to_offset(time::UtcOffset::UTC);
        if let Err(error) = validate_report_identity(&val.raw_text, &val.station_id, generated_at) {
            problems.push(ALL, error.to_string());
        }
        let quality_status = if !problems.reasons.is_empty() {
            "rejected"
        } else if raw_temperatures(&val.raw_text).is_some() {
            "validated"
        } else {
            problems.push(
                TEMPERATURES,
                "no unambiguous raw METAR temperature evidence",
            );
            "unverified"
        };
        Ok(CurrentWeather {
            raw_text: val.raw_text.clone(),
            metar_type,
            quality_status: quality_status.into(),
            quality_reason: (!problems.reasons.is_empty()).then(|| problems.reasons.join("; ")),
            quality_metrics: (!problems.groups.is_empty()).then(|| {
                problems
                    .groups
                    .iter()
                    .map(|group| group.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            }),
            station_id: val.station_id,
            latitude,
            longitude,
            generated_at,
            temperature_value,
            temperature_unit_code: Units::Celcius.to_string(),
            wind_direction,
            wind_direction_unit_code: Units::DegreesTrue.to_string(),
            wind_speed,
            wind_speed_unit_code: Units::Knots.to_string(),
            dewpoint_value,
            dewpoint_unit_code: Units::Celcius.to_string(),
            precip_in: if special_without_precip && decoded_precip.is_none_or(|value| value == 0.0)
            {
                None
            } else if val.precip_in.is_some() {
                decoded_precip
            } else {
                precipitation(None, &val.raw_text)
            },
            precip_unit_code: Units::Inches.to_string(),
            wx_string: val.wx_string.unwrap_or_default(),
        })
    }
}

const VALIDATION_VERSION: &str = "metar-consistency-v1";

/// The measurements a quality problem can make wrong. The oracle maps its
/// metrics onto these, so a problem outside an event's metrics, such as a
/// rain gauge outage for a temperature event, does not hold its settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MetricGroup {
    Temperature,
    Dewpoint,
    Wind,
    Precipitation,
    PresentWeather,
}

impl MetricGroup {
    pub fn as_str(self) -> &'static str {
        match self {
            MetricGroup::Temperature => "temperature",
            MetricGroup::Dewpoint => "dewpoint",
            MetricGroup::Wind => "wind",
            MetricGroup::Precipitation => "precipitation",
            MetricGroup::PresentWeather => "present_weather",
        }
    }
}

const TEMPERATURE: &[MetricGroup] = &[MetricGroup::Temperature];
const DEWPOINT: &[MetricGroup] = &[MetricGroup::Dewpoint];
/// The body and T groups report temperature and dewpoint together.
const TEMPERATURES: &[MetricGroup] = &[MetricGroup::Temperature, MetricGroup::Dewpoint];
const WIND: &[MetricGroup] = &[MetricGroup::Wind];
const PRECIPITATION: &[MetricGroup] = &[MetricGroup::Precipitation];
const PRESENT_WEATHER: &[MetricGroup] = &[MetricGroup::PresentWeather];
/// A report whose identity or type is in doubt cannot vouch for any value.
const ALL: &[MetricGroup] = &[
    MetricGroup::Temperature,
    MetricGroup::Dewpoint,
    MetricGroup::Wind,
    MetricGroup::Precipitation,
    MetricGroup::PresentWeather,
];

/// A report's quality problems and the metric groups they affect.
#[derive(Default)]
struct Problems {
    reasons: Vec<String>,
    groups: BTreeSet<MetricGroup>,
}

impl Problems {
    fn push(&mut self, groups: &[MetricGroup], reason: impl Into<String>) {
        self.reasons.push(reason.into());
        self.groups.extend(groups);
    }
}
#[cfg(test)]
const OBSERVATION_SOURCE: &str = "https://aviationweather.gov/data/cache/metars.cache.xml.gz";

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_report_identity(
    raw: &str,
    station_id: &str,
    timestamp: OffsetDateTime,
) -> Result<(), Error> {
    if raw.trim().is_empty() {
        return Ok(());
    }
    let mut tokens = raw.split_whitespace();
    let first = tokens.next().unwrap_or_default();
    let raw_station = if matches!(first, "METAR" | "SPECI") {
        tokens.next().unwrap_or_default()
    } else {
        first
    };
    if raw_station != station_id {
        return Err(anyhow!(
            "raw METAR station {raw_station:?} differs from decoded station {station_id:?}"
        ));
    }
    let raw_time = tokens.next().unwrap_or_default();
    let expected = format!(
        "{:02}{:02}{:02}Z",
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute()
    );
    if raw_time != expected {
        return Err(anyhow!(
            "raw METAR time {raw_time:?} differs from decoded day/time {expected}"
        ));
    }
    Ok(())
}

fn checked_number(
    value: Option<&str>,
    field: &str,
    groups: &[MetricGroup],
    problems: &mut Problems,
) -> Option<f64> {
    let value = value?;
    match value.trim().parse::<f64>() {
        Ok(number) if number.is_finite() => Some(number),
        _ => {
            problems.push(groups, format!("invalid {field}: {value:?}"));
            None
        }
    }
}

fn checked_integer(
    value: Option<&str>,
    field: &str,
    groups: &[MetricGroup],
    problems: &mut Problems,
) -> Option<i64> {
    let value = value?;
    match value.trim().parse::<i64>() {
        Ok(number) => Some(number),
        Err(_) => {
            problems.push(groups, format!("invalid {field}: {value:?}"));
            None
        }
    }
}

fn required_coordinate(value: Option<&str>, field: &str, min: f64, max: f64) -> Result<f64, Error> {
    let number = value
        .ok_or_else(|| anyhow!("missing {field}"))?
        .parse::<f64>()?;
    if !number.is_finite() || !(min..=max).contains(&number) {
        return Err(anyhow!("invalid {field}: {number}"));
    }
    Ok(number)
}

/// Reject a decoded value that contradicts an unambiguous group in the original report.
/// Keep the upstream values unchanged when they agree; missing raw or decoded values do
/// not supply replacement readings. The caller retains rejected reports with flags.
fn validate_temperatures(
    raw_text: &str,
    temperature: Option<f64>,
    dewpoint: Option<f64>,
) -> Result<(), Error> {
    let Some(raw) = raw_temperatures(raw_text) else {
        return Ok(());
    };
    for (name, decoded, reported) in [
        ("temperature", temperature, Some(raw.temperature)),
        ("dewpoint", dewpoint, raw.dewpoint),
    ] {
        if let (Some(decoded), Some(reported)) = (decoded, reported) {
            // The body rounds to whole Celsius degrees; T remarks give tenths. Allow
            // whole-degree decoded values too, including half-degree negative ties.
            let rounding_tolerance = if raw.token.starts_with('T') && decoded.fract() != 0.0 {
                1e-6
            } else {
                0.5 + 1e-6
            };
            if !decoded.is_finite() || (decoded - reported).abs() > rounding_tolerance {
                return Err(anyhow!(
                    "decoded {name} {decoded} C conflicts with raw METAR group {} ({reported} C)",
                    raw.token,
                ));
            }
        }
    }
    Ok(())
}

struct RawTemperatures<'a> {
    token: &'a str,
    temperature: f64,
    dewpoint: Option<f64>,
}

/// AWC describes the whole-degree body group and the signed tenths T remark at
/// https://aviationweather.gov/help/data/ . Prefer a unique T remark. Duplicate
/// groups are ambiguous, so they cannot produce validated readings.
fn raw_temperatures(raw_text: &str) -> Option<RawTemperatures<'_>> {
    let (mut body, mut precise) = (None, None);
    let (mut body_count, mut precise_count) = (0, 0);
    let mut remarks = false;
    for token in raw_text.split_whitespace() {
        let token = token.trim_end_matches('=');
        if token == "RMK" {
            remarks = true;
            continue;
        }
        let values = if remarks {
            let Some(digits) = token.strip_prefix('T') else {
                continue;
            };
            if !matches!(digits.len(), 4 | 8) || !digits.is_ascii() {
                continue;
            }
            precise_temperature(&digits[..4]).and_then(|temperature| {
                let dewpoint = if digits.len() == 8 {
                    Some(precise_temperature(&digits[4..])?)
                } else {
                    None
                };
                Some((temperature, dewpoint))
            })
        } else {
            let Some((temperature, dewpoint)) = token.split_once('/') else {
                continue;
            };
            body_temperature(temperature).and_then(|temperature| {
                let dewpoint = if dewpoint.is_empty() {
                    None
                } else {
                    Some(body_temperature(dewpoint)?)
                };
                Some((temperature, dewpoint))
            })
        };
        if let Some((temperature, dewpoint)) = values {
            let raw = RawTemperatures {
                token,
                temperature,
                dewpoint,
            };
            if remarks {
                precise = Some(raw);
                precise_count += 1;
            } else {
                body = Some(raw);
                body_count += 1;
            }
        }
    }
    if precise_count > 1 || body_count > 1 {
        return None;
    }
    if let (Some(body), Some(precise)) = (&body, &precise)
        && ((body.temperature - precise.temperature).abs() > 0.5 + 1e-6
            || body
                .dewpoint
                .zip(precise.dewpoint)
                .is_some_and(|(a, b)| (a - b).abs() > 0.5 + 1e-6))
    {
        return None;
    }
    match (precise_count, body_count) {
        (1, _) => precise,
        (0, 1) => body,
        _ => None,
    }
}

fn body_temperature(token: &str) -> Option<f64> {
    let digits = token.strip_prefix('M').unwrap_or(token);
    // Exact two-digit groups exclude malformed wind tokens such as 060/03.
    if digits.len() != 2 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value: f64 = digits.parse().ok()?;
    Some(if token.starts_with('M') {
        -value
    } else {
        value
    })
}

fn precise_temperature(token: &str) -> Option<f64> {
    if token.len() != 4 || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value = token[1..].parse::<f64>().ok()? / 10.0;
    match token.as_bytes()[0] {
        b'0' => Some(value),
        b'1' => Some(-value),
        _ => None,
    }
}

/// Prrrr encodes the hourly amount in hundredths of an inch. P0000 is
/// trace, so decoded 0.005 is a source trace convention, not an exact amount.
fn raw_precipitation(raw_text: &str) -> Result<Option<Option<f64>>, Error> {
    let mut found = None;
    for token in raw_text
        .split_whitespace()
        .skip_while(|token| *token != "RMK")
        .skip(1)
    {
        let token = token.trim_end_matches('=');
        let Some(digits) = token.strip_prefix('P') else {
            continue;
        };
        if digits.len() != 4
            || !(digits == "////" || digits.bytes().all(|byte| byte.is_ascii_digit()))
        {
            continue;
        }
        if found.is_some() {
            return Err(anyhow!("ambiguous duplicate hourly precipitation groups"));
        }
        found = Some(if digits == "////" {
            None
        } else {
            Some(digits.parse::<f64>()? / 100.0)
        });
    }
    Ok(found)
}

fn validate_precipitation(raw_text: &str, decoded: Option<f64>) -> Result<(), Error> {
    match (raw_precipitation(raw_text)?, decoded) {
        (Some(Some(reported)), Some(decoded)) => {
            let matches = if reported == 0.0 {
                (0.0..0.01).contains(&decoded)
            } else {
                (reported - decoded).abs() < 1e-6
            };
            if !matches {
                return Err(anyhow!(
                    "decoded precipitation {decoded} inches conflicts with raw hourly P group ({reported} inches; zero group means trace)"
                ));
            }
        }
        (Some(Some(_)), None) => {
            return Err(anyhow!(
                "raw hourly precipitation is present but decoded precip_in is missing"
            ));
        }
        (Some(None), _) => return Err(anyhow!("raw hourly precipitation is unavailable (P////)")),
        (None, _) => {}
    }
    Ok(())
}

/// Hourly precipitation in inches. At AO2 automated stations the METAR
/// precipitation group is omitted from routine METAR when none fell, so
/// absence means 0. In SPECI, elsewhere, or with a PNO rain-gauge outage,
/// absence means unknown. `VRB` and absent wind directions likewise stay
/// null rather than becoming 0 (north).
fn precipitation(precip_in: Option<&str>, raw_text: &str) -> Option<f64> {
    match precip_in.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.parse::<f64>().ok().filter(|value| {
            !(*value == 0.0
                && raw_text.split_whitespace().next() == Some("SPECI")
                && matches!(raw_precipitation(raw_text), Ok(None)))
        }),
        None => (is_ao2(raw_text)
            && raw_text.split_whitespace().next() != Some("SPECI")
            && !has_remark(raw_text, "PNO")
            && matches!(raw_precipitation(raw_text), Ok(None)))
        .then_some(0.0),
    }
}

/// Whether the report's remarks declare an AO2 station (precipitation
/// discriminator), e.g. `KORD 032151Z 23006KT ... RMK AO2 SLP162`.
fn is_ao2(raw_text: &str) -> bool {
    has_remark(raw_text, "AO2")
}

fn has_remark(raw_text: &str, expected: &str) -> bool {
    raw_text
        .split_whitespace()
        .skip_while(|token| *token != "RMK")
        .skip(1)
        .any(|token| token.trim_end_matches('=') == expected)
}

/// What one observation run wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationReport {
    pub written: usize,
    /// Reports that could not be represented; any such configured station fails the run.
    pub skipped: usize,
    pub rejected: usize,
    pub unverified: usize,
}

#[cfg(test)]
#[path = "metar_temperature_tests.rs"]
mod temperature_tests;

#[derive(Debug, ParquetRecordWriter)]
pub struct Observation {
    pub station_id: String,
    pub station_name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub generated_at: String,
    pub temperature_value: Option<f64>,
    pub temperature_unit_code: String,
    pub wind_direction: Option<i64>,
    pub wind_direction_unit_code: String,
    pub wind_speed: Option<i64>,
    pub wind_speed_unit_code: String,
    pub dewpoint_value: Option<f64>,
    pub dewpoint_unit_code: String,
    // New fields at the end for backwards compatibility
    pub state: String,
    pub iata_id: String,
    pub elevation_m: Option<f64>,
    pub precip_in: Option<f64>,
    pub precip_unit_code: String,
    pub wx_string: String,
    pub raw_text: Option<String>,
    pub quality_status: Option<String>,
    pub quality_reason: Option<String>,
    pub validation_version: Option<String>,
    pub metar_type: Option<String>,
    pub quality_metrics: Option<String>,
}

impl TryFrom<CurrentWeather> for Observation {
    type Error = anyhow::Error;
    fn try_from(val: CurrentWeather) -> Result<Self, Self::Error> {
        let rfc_3339_time_description =
            format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
        let parquet = Observation {
            station_id: val.station_id,
            station_name: String::from(""),
            latitude: val.latitude,
            longitude: val.longitude,
            generated_at: val
                .generated_at
                .format(rfc_3339_time_description)
                .map_err(|e| anyhow!("error formatting generated_at time: {}", e))?,
            temperature_value: val.temperature_value,
            temperature_unit_code: val.temperature_unit_code,
            wind_speed: val.wind_speed,
            wind_speed_unit_code: val.wind_speed_unit_code,
            wind_direction: val.wind_direction,
            wind_direction_unit_code: val.wind_direction_unit_code,
            dewpoint_value: val.dewpoint_value,
            dewpoint_unit_code: val.dewpoint_unit_code,
            // New fields
            state: String::from(""),
            iata_id: String::from(""),
            elevation_m: None,
            precip_in: val.precip_in,
            precip_unit_code: val.precip_unit_code,
            wx_string: val.wx_string,
            raw_text: Some(val.raw_text),
            quality_status: Some(val.quality_status),
            quality_reason: val.quality_reason,
            validation_version: Some(VALIDATION_VERSION.into()),
            metar_type: val.metar_type,
            quality_metrics: val.quality_metrics,
        };
        Ok(parquet)
    }
}

pub fn create_observation_schema() -> Type {
    let station_id = Type::primitive_type_builder("station_id", PhysicalType::BYTE_ARRAY)
        .with_repetition(Repetition::REQUIRED)
        .with_logical_type(Some(LogicalType::String))
        .build()
        .unwrap();

    let station_name = Type::primitive_type_builder("station_name", PhysicalType::BYTE_ARRAY)
        .with_repetition(Repetition::REQUIRED)
        .with_logical_type(Some(LogicalType::String))
        .build()
        .unwrap();

    let latitude = Type::primitive_type_builder("latitude", PhysicalType::DOUBLE)
        .with_repetition(Repetition::REQUIRED)
        .build()
        .unwrap();

    let longitude = Type::primitive_type_builder("longitude", PhysicalType::DOUBLE)
        .with_repetition(Repetition::REQUIRED)
        .build()
        .unwrap();

    let generated_at = Type::primitive_type_builder("generated_at", PhysicalType::BYTE_ARRAY)
        .with_logical_type(Some(LogicalType::String))
        .with_repetition(Repetition::REQUIRED)
        .build()
        .unwrap();

    let temperature_value = Type::primitive_type_builder("temperature_value", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let temperature_unit_code =
        Type::primitive_type_builder("temperature_unit_code", PhysicalType::BYTE_ARRAY)
            .with_repetition(Repetition::REQUIRED)
            .with_logical_type(Some(LogicalType::String))
            .build()
            .unwrap();

    let wind_direction = Type::primitive_type_builder("wind_direction", PhysicalType::INT64)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let wind_direction_unit_code =
        Type::primitive_type_builder("wind_direction_unit_code", PhysicalType::BYTE_ARRAY)
            .with_repetition(Repetition::REQUIRED)
            .with_logical_type(Some(LogicalType::String))
            .build()
            .unwrap();

    let wind_speed = Type::primitive_type_builder("wind_speed", PhysicalType::INT64)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let wind_speed_unit_code =
        Type::primitive_type_builder("wind_speed_unit_code", PhysicalType::BYTE_ARRAY)
            .with_repetition(Repetition::REQUIRED)
            .with_logical_type(Some(LogicalType::String))
            .build()
            .unwrap();

    let dewpoint_value = Type::primitive_type_builder("dewpoint_value", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let dewpoint_unit_code =
        Type::primitive_type_builder("dewpoint_unit_code", PhysicalType::BYTE_ARRAY)
            .with_repetition(Repetition::REQUIRED)
            .with_logical_type(Some(LogicalType::String))
            .build()
            .unwrap();

    // New fields at the end for backwards compatibility
    let state = Type::primitive_type_builder("state", PhysicalType::BYTE_ARRAY)
        .with_repetition(Repetition::REQUIRED)
        .with_logical_type(Some(LogicalType::String))
        .build()
        .unwrap();

    let iata_id = Type::primitive_type_builder("iata_id", PhysicalType::BYTE_ARRAY)
        .with_repetition(Repetition::REQUIRED)
        .with_logical_type(Some(LogicalType::String))
        .build()
        .unwrap();

    let elevation_m = Type::primitive_type_builder("elevation_m", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let precip_in = Type::primitive_type_builder("precip_in", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let precip_unit_code =
        Type::primitive_type_builder("precip_unit_code", PhysicalType::BYTE_ARRAY)
            .with_repetition(Repetition::REQUIRED)
            .with_logical_type(Some(LogicalType::String))
            .build()
            .unwrap();

    let wx_string = Type::primitive_type_builder("wx_string", PhysicalType::BYTE_ARRAY)
        .with_repetition(Repetition::REQUIRED)
        .with_logical_type(Some(LogicalType::String))
        .build()
        .unwrap();

    Type::group_type_builder("observation")
        .with_fields(vec![
            Arc::new(station_id),
            Arc::new(station_name),
            Arc::new(latitude),
            Arc::new(longitude),
            Arc::new(generated_at),
            Arc::new(temperature_value),
            Arc::new(temperature_unit_code),
            Arc::new(wind_direction),
            Arc::new(wind_direction_unit_code),
            Arc::new(wind_speed),
            Arc::new(wind_speed_unit_code),
            Arc::new(dewpoint_value),
            Arc::new(dewpoint_unit_code),
            // New fields at end
            Arc::new(state),
            Arc::new(iata_id),
            Arc::new(elevation_m),
            Arc::new(precip_in),
            Arc::new(precip_unit_code),
            Arc::new(wx_string),
            optional_text("raw_text"),
            optional_text("quality_status"),
            optional_text("quality_reason"),
            optional_text("validation_version"),
            optional_text("metar_type"),
            optional_text("quality_metrics"),
        ])
        .build()
        .unwrap()
}

fn optional_text(name: &str) -> Arc<Type> {
    Arc::new(
        Type::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
            .with_repetition(Repetition::OPTIONAL)
            .with_logical_type(Some(LogicalType::String))
            .build()
            .expect("optional UTF8 schema"),
    )
}

#[derive(Serialize)]
struct RejectedReport<'a> {
    reason: String,
    report: &'a Metar,
}

#[derive(Serialize)]
struct ObservationAudit<'a> {
    validation_version: &'static str,
    source_url: &'static str,
    source_sha256: String,
    source_hash_kind: &'static str,
    source_documents: Option<&'a BTreeMap<String, ArchivedResponse>>,
    coverage: Option<&'a ObservationCoverage>,
    precipitation: Option<&'a super::shef::ShefCollection>,
    fetched_at: String,
    received: usize,
    out_of_scope: usize,
    written: usize,
    rejected: usize,
    unverified: usize,
    unrepresentable: usize,
    issues: Vec<RejectedReport<'a>>,
}

/// Write and sync evidence before creating publishable data. Sidecars never pass
/// Artifact::from_path; failed runs retain evidence instead of publishing partial rows.
fn persist_audit(path: &Path, audit: &ObservationAudit<'_>) -> Result<String, Error> {
    let json = serde_json::to_string(audit)?;
    let mut file = File::create(path)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        File::open(parent)?.sync_all()?;
    }
    Ok(json)
}

pub struct ObservationService {
    pub logger: Logger,
    pub fetcher: Arc<XmlFetcher>,
}
impl ObservationService {
    pub fn new(logger: Logger, fetcher: Arc<XmlFetcher>) -> Self {
        ObservationService { logger, fetcher }
    }

    /// Collect all reports in a bounded history window, preserving successful,
    /// empty and failed query receipts in the same published artifact.
    /// `retry` holds earlier runs' failed intervals, requested after the
    /// window; it is replaced with the intervals this run failed.
    pub async fn get_observations_to_file(
        &self,
        city_weather: &CityWeather,
        output_path: &str,
        started_at: OffsetDateTime,
        config: &HistoryConfig,
        catalog: &crate::coordinates::StationCatalogEvidence,
        retry: &mut Vec<HistoryQuery>,
    ) -> Result<ObservationReport, Error> {
        info!(
            self.logger,
            "fetching {} hours of observation history and {} earlier failed intervals",
            config.hours,
            retry.len()
        );
        let earlier = std::mem::take(retry);
        let mut history = match collect_history(
            self.fetcher.clone(),
            city_weather.city_data.keys().cloned().collect(),
            started_at,
            config,
            earlier.clone(),
        )
        .await
        {
            Ok(history) => history,
            Err(error) => {
                let mut history = HistoryCollection::unavailable(
                    started_at,
                    config,
                    format!("history collection failed: {error:#}"),
                )?;
                history.retry.extend(earlier);
                history
            }
        };
        retry.clone_from(&history.retry);
        let complete = history
            .coverage
            .batches
            .iter()
            .filter(|batch| batch.status == "complete")
            .count();
        let empty = history
            .coverage
            .batches
            .iter()
            .filter(|batch| batch.status == "empty")
            .count();
        let failed = history.coverage.batches.len() - complete - empty;
        info!(
            self.logger,
            "observation query receipts: {} complete, {} empty, {} failed", complete, empty, failed
        );
        history.precipitation = Some(
            super::shef::collect(
                self.fetcher.clone(),
                city_weather,
                catalog,
                started_at,
                config.hours,
            )
            .await,
        );
        self.write_history(city_weather, output_path, &history)
    }

    /// Adds the whole window to `retry`, since no station was requested.
    pub fn write_unavailable_history(
        &self,
        output_path: &str,
        started_at: OffsetDateTime,
        config: &HistoryConfig,
        error: String,
        retry: &mut Vec<HistoryQuery>,
    ) -> Result<ObservationReport, Error> {
        let history = HistoryCollection::unavailable(started_at, config, error)?;
        retry.extend(history.retry.iter().cloned());
        self.write_history(
            &CityWeather {
                city_data: Default::default(),
            },
            output_path,
            &history,
        )
    }

    pub(super) fn write_history(
        &self,
        city_weather: &CityWeather,
        output_path: &str,
        history: &HistoryCollection,
    ) -> Result<ObservationReport, Error> {
        let source_manifest = serde_json::to_vec(&history.sources)?;
        let audit = ObservationAudit {
            validation_version: VALIDATION_VERSION,
            source_url: HISTORY_SOURCE,
            source_sha256: sha256_hex(&source_manifest),
            source_hash_kind: "source_documents-json-sha256",
            source_documents: Some(&history.sources),
            coverage: Some(&history.coverage),
            precipitation: history.precipitation.as_ref(),
            fetched_at: history.coverage.completed_at.clone(),
            received: history.reports.len(),
            out_of_scope: 0,
            written: 0,
            rejected: 0,
            unverified: 0,
            unrepresentable: 0,
            issues: Vec::new(),
        };
        self.write_reports(city_weather, output_path, &history.reports, audit)
    }

    #[cfg(test)]
    fn write_observations(
        &self,
        city_weather: &CityWeather,
        output_path: &str,
        raw_observation: &str,
        converted_xml: &ObservationData,
    ) -> Result<ObservationReport, Error> {
        let audit = ObservationAudit {
            validation_version: VALIDATION_VERSION,
            source_url: OBSERVATION_SOURCE,
            source_sha256: sha256_hex(raw_observation.as_bytes()),
            source_hash_kind: "response-body-sha256",
            source_documents: None,
            coverage: None,
            precipitation: None,
            fetched_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
            received: converted_xml.data.metar.len(),
            out_of_scope: 0,
            written: 0,
            rejected: 0,
            unverified: 0,
            unrepresentable: 0,
            issues: Vec::new(),
        };
        self.write_reports(city_weather, output_path, &converted_xml.data.metar, audit)
    }

    fn write_reports<'a>(
        &self,
        city_weather: &CityWeather,
        output_path: &str,
        reports: &'a [Metar],
        mut audit: ObservationAudit<'a>,
    ) -> Result<ObservationReport, Error> {
        let mut observations = vec![];
        for value in reports {
            let Some(city) = city_weather.city_data.get(&value.station_id) else {
                audit.out_of_scope += 1;
                continue;
            };
            let converted = CurrentWeather::try_from(value.clone()).and_then(Observation::try_from);
            let mut observation = match converted {
                Ok(observation) => observation,
                Err(error) => {
                    audit.unrepresentable += 1;
                    audit.issues.push(RejectedReport {
                        reason: error.to_string(),
                        report: value,
                    });
                    continue;
                }
            };
            match observation.quality_status.as_deref() {
                Some("rejected") => audit.rejected += 1,
                Some("unverified") => audit.unverified += 1,
                _ => {}
            }
            if let Some(reason) = observation.quality_reason.as_ref() {
                audit.issues.push(RejectedReport {
                    reason: reason.clone(),
                    report: value,
                });
            }
            observation.station_name = city.station_name.clone();
            observation.state = city.state.clone();
            observation.iata_id = city.iata_id.clone();
            observation.elevation_m = city.elevation_m;
            observations.push(observation);
        }
        audit.written = observations.len();
        let sidecar = format!("{output_path}.quality.json");
        let audit_json = persist_audit(Path::new(&sidecar), &audit)?;
        if audit.unrepresentable > 0 && audit.coverage.is_none() {
            return Err(anyhow!(
                "{} configured METAR reports cannot be represented; publication refused; evidence: {}",
                audit.unrepresentable,
                sidecar
            ));
        }
        if audit.rejected + audit.unverified > 0 {
            warn!(
                self.logger,
                "observation data quality: {} rejected, {} unverified; evidence: {}",
                audit.rejected,
                audit.unverified,
                sidecar
            );
        }
        let (output, file) = PartialFile::create(output_path)
            .map_err(|e| anyhow!("failed to create parquet file: {}", e))?;
        let mut metadata = vec![
            KeyValue::new("source_url".into(), Some(audit.source_url.into())),
            KeyValue::new("source_sha256".into(), Some(audit.source_sha256.clone())),
            KeyValue::new("validation_version".into(), Some(VALIDATION_VERSION.into())),
            KeyValue::new("observation_audit".into(), Some(audit_json)),
        ];
        if let Some(coverage) = audit.coverage {
            metadata.push(KeyValue::new(
                "observation_coverage".into(),
                Some(serde_json::to_string(coverage)?),
            ));
        }
        if let Some(precipitation) = audit.precipitation {
            metadata.push(KeyValue::new(
                "precipitation_observations".into(),
                Some(serde_json::to_string(precipitation)?),
            ));
        }
        let props = parquet_file::properties()
            .set_key_value_metadata(Some(metadata))
            .build();
        let mut writer =
            SerializedFileWriter::new(file, Arc::new(create_observation_schema()), Arc::new(props))
                .map_err(|e| anyhow!("failed to create parquet writer: {}", e))?;

        // Write all observations as a single row group
        info!(
            self.logger,
            "writing {} observations to {}",
            observations.len(),
            output_path
        );
        let mut row_group = writer
            .next_row_group()
            .map_err(|e| anyhow!("failed to create row group: {}", e))?;
        observations
            .as_slice()
            .write_to_row_group(&mut row_group)
            .map_err(|e| anyhow!("failed to write observations: {}", e))?;
        row_group
            .close()
            .map_err(|e| anyhow!("failed to close row group: {}", e))?;
        writer
            .close()
            .map_err(|e| anyhow!("failed to close parquet writer: {}", e))?;
        output
            .commit()
            .map_err(|e| anyhow!("failed to finish parquet file: {}", e))?;

        info!(self.logger, "done writing observations to {}", output_path);
        Ok(ObservationReport {
            written: observations.len(),
            skipped: audit.unrepresentable,
            rejected: audit.rejected,
            unverified: audit.unverified,
        })
    }
}

#[cfg(test)]
mod precipitation_tests {
    use super::*;

    const AO2: &str = "KORD 032151Z 23006KT 10SM BKN110 14/03 A3000 RMK AO2 SLP162 T01440028";
    const AO1: &str = "KXYZ 032151Z 23006KT 10SM BKN110 14/03 A3000 RMK AO1";

    #[test]
    fn absent_precipitation_is_zero_only_at_ao2_stations() {
        assert_eq!(precipitation(None, AO2), Some(0.0));
        assert_eq!(precipitation(Some(""), AO2), Some(0.0));
        assert_eq!(precipitation(None, AO1), None);
        assert_eq!(precipitation(None, "KXYZ 032151Z AUTO 23006KT"), None);
        assert_eq!(precipitation(Some("0.02"), AO1), Some(0.02));
        assert_eq!(precipitation(Some("0.005"), AO2), Some(0.005), "trace");
        assert!(!is_ao2("KXYZ 032151Z AO2 RMK AO1"), "only remarks count");
    }
}
