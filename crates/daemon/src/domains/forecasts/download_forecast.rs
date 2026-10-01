use crate::Type::{
    Ice, Liquid, Maximum, MaximumRelative, Minimum, MinimumRelative,
    ProbabilityOfPrecipitationWithin12Hours, Snow, SnowRatio, Sustained, Wind,
};
use crate::parquet_file::{self, PartialFile};
use crate::{CityWeather, DataReading, Dwml, Units, WeatherStation, XmlFetcher, split_cityweather};
use anyhow::{Error, anyhow};
use async_compression::tokio::write::GzipEncoder;
use base64::Engine;
use core::time::Duration as StdDuration;
use futures::stream::{self, StreamExt};
use parquet::basic::LogicalType;
use parquet::file::metadata::KeyValue;
use parquet::file::writer::SerializedFileWriter;
use parquet::record::RecordWriter;
use parquet::{
    basic::{Repetition, Type as PhysicalType},
    schema::types::Type,
};
use parquet_derive::ParquetRecordWriter;
use sha2::{Digest, Sha256};
use slog::{Logger, error, info};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use time::{
    Duration, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339,
    macros::format_description,
};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
/*
More Options defined  here:
https://graphical.weather.gov/xml/docs/elementInputNames.php

Maximum Temperature 	maxt
Minimum Temperature 	mint
Wind Speed 	wspd
Wind Direction 	wdir
12 Hour Probability of Precipitation 	pop12
Liquid Precipitation Amount 	qpf
Maximum Relative Humidity 	maxrh
Minimum Relative Humidity 	minrh
*/
#[derive(Debug, Clone)]
pub struct WeatherForecast {
    pub station_id: String,
    pub station_name: String,
    pub latitude: String,
    pub longitude: String,
    pub generated_at: OffsetDateTime,
    pub begin_time: OffsetDateTime,
    pub end_time: OffsetDateTime,
    pub max_temp: Option<i64>,
    pub min_temp: Option<i64>,
    pub temperature_unit_code: String,
    pub wind_speed: Option<i64>,
    pub wind_speed_unit_code: String,
    pub wind_direction: Option<i64>,
    pub wind_direction_unit_code: String,
    pub relative_humidity_max: Option<i64>,
    pub relative_humidity_min: Option<i64>,
    pub relative_humidity_unit_code: String,
    pub liquid_precipitation_amt: Option<f64>,
    pub liquid_precipitation_unit_code: String,
    pub snow_amt: Option<f64>,
    pub snow_amt_unit_code: String,
    pub snow_ratio: Option<f64>,
    pub snow_ratio_unit_code: String,
    pub ice_amt: Option<f64>,
    pub ice_amt_unit_code: String,
    pub twelve_hour_probability_of_precipitation: Option<i64>,
    pub twelve_hour_probability_of_precipitation_unit_code: String,
    pub provenance: ForecastProvenance,
}

const FORECAST_INTERVAL_VERSION: &str = "ndfd-native-v1";

#[derive(Debug, Clone, Default)]
pub struct ForecastProvenance {
    pub source_url: Option<String>,
    pub received_at: Option<String>,
    pub xml_sha256: Option<String>,
    pub location: String,
    pub layouts: BTreeMap<String, NativeValueProvenance>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NativeValueProvenance {
    pub layout: String,
    pub start: String,
    pub end: Option<String>,
    pub index: usize,
    pub value: String,
    pub units: String,
}

#[derive(ParquetRecordWriter, Debug)]
pub struct Forecast {
    pub station_id: String,
    pub station_name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub generated_at: String,
    pub begin_time: String,
    pub end_time: String,
    pub max_temp: Option<i64>,
    pub min_temp: Option<i64>,
    pub temperature_unit_code: String,
    pub wind_speed: Option<i64>,
    pub wind_speed_unit_code: String,
    pub wind_direction: Option<i64>,
    pub wind_direction_unit_code: String,
    pub relative_humidity_max: Option<i64>,
    pub relative_humidity_min: Option<i64>,
    pub relative_humidity_unit_code: String,
    pub liquid_precipitation_amt: Option<f64>,
    pub liquid_precipitation_unit_code: String,
    pub twelve_hour_probability_of_precipitation: Option<i64>,
    pub twelve_hour_probability_of_precipitation_unit_code: String,
    // New fields at the end for backwards compatibility
    pub state: String,
    pub iata_id: String,
    pub elevation_m: Option<f64>,
    pub snow_amt: Option<f64>,
    pub snow_amt_unit_code: String,
    pub snow_ratio: Option<f64>,
    pub snow_ratio_unit_code: String,
    pub ice_amt: Option<f64>,
    pub ice_amt_unit_code: String,
    // Native intervals and source evidence. Historical files omit these columns.
    pub forecast_interval_version: Option<String>,
    pub interval_kind: Option<String>,
    pub source_url: Option<String>,
    pub source_received_at: Option<String>,
    pub source_xml_sha256: Option<String>,
    pub source_location: Option<String>,
    pub source_layouts: Option<String>,
    pub quality_status: Option<String>,
    pub quality_reason: Option<String>,
}

impl TryFrom<WeatherForecast> for Forecast {
    type Error = anyhow::Error;
    fn try_from(val: WeatherForecast) -> Result<Self, Self::Error> {
        let latitude = val.latitude.parse::<f64>()?;
        let longitude = val.longitude.parse::<f64>()?;
        if !latitude.is_finite()
            || !longitude.is_finite()
            || !(-90.0..=90.0).contains(&latitude)
            || !(-180.0..=180.0).contains(&longitude)
        {
            return Err(anyhow!("invalid forecast coordinates"));
        }
        let parquet = Forecast {
            station_id: val.station_id,
            station_name: String::from(""),
            latitude,
            longitude,
            generated_at: val
                .generated_at
                .format(&Rfc3339)
                .map_err(|e| anyhow!("error formatting generated_at time: {}", e))?,
            begin_time: val
                .begin_time
                .format(&Rfc3339)
                .map_err(|e| anyhow!("error formatting begin time: {}", e))?,
            end_time: val
                .end_time
                .format(&Rfc3339)
                .map_err(|e| anyhow!("error formatting end time: {}", e))?,
            max_temp: val.max_temp,
            min_temp: val.min_temp,
            temperature_unit_code: val.temperature_unit_code,
            wind_speed: val.wind_speed,
            wind_speed_unit_code: val.wind_speed_unit_code,
            wind_direction: val.wind_direction,
            wind_direction_unit_code: val.wind_direction_unit_code,
            relative_humidity_max: val.relative_humidity_max,
            relative_humidity_min: val.relative_humidity_min,
            relative_humidity_unit_code: val.relative_humidity_unit_code,
            liquid_precipitation_amt: val.liquid_precipitation_amt,
            liquid_precipitation_unit_code: val.liquid_precipitation_unit_code,
            twelve_hour_probability_of_precipitation: val.twelve_hour_probability_of_precipitation,
            twelve_hour_probability_of_precipitation_unit_code: val
                .twelve_hour_probability_of_precipitation_unit_code,
            // New fields
            state: String::from(""),
            iata_id: String::from(""),
            elevation_m: None,
            snow_amt: val.snow_amt,
            snow_amt_unit_code: val.snow_amt_unit_code,
            snow_ratio: val.snow_ratio,
            snow_ratio_unit_code: val.snow_ratio_unit_code,
            ice_amt: val.ice_amt,
            ice_amt_unit_code: val.ice_amt_unit_code,
            forecast_interval_version: Some(FORECAST_INTERVAL_VERSION.into()),
            interval_kind: Some(
                if val.begin_time == val.end_time {
                    "instant"
                } else {
                    "period"
                }
                .into(),
            ),
            source_url: val.provenance.source_url,
            source_received_at: val.provenance.received_at,
            source_xml_sha256: val.provenance.xml_sha256,
            source_location: Some(val.provenance.location),
            source_layouts: Some(serde_json::to_string(&val.provenance.layouts)?),
            quality_status: Some(
                if val.provenance.problems.is_empty() {
                    "validated"
                } else {
                    "rejected"
                }
                .into(),
            ),
            quality_reason: (!val.provenance.problems.is_empty())
                .then(|| val.provenance.problems.join("; ")),
        };
        Ok(parquet)
    }
}

pub fn create_forecast_schema() -> Result<Type, parquet::errors::ParquetError> {
    let station_id = Type::primitive_type_builder("station_id", PhysicalType::BYTE_ARRAY)
        .with_logical_type(Some(LogicalType::String))
        .with_repetition(Repetition::REQUIRED)
        .build()?;

    let station_name = Type::primitive_type_builder("station_name", PhysicalType::BYTE_ARRAY)
        .with_repetition(Repetition::REQUIRED)
        .with_logical_type(Some(LogicalType::String))
        .build()?;

    let latitude = Type::primitive_type_builder("latitude", PhysicalType::DOUBLE)
        .with_repetition(Repetition::REQUIRED)
        .build()?;

    let longitude = Type::primitive_type_builder("longitude", PhysicalType::DOUBLE)
        .with_repetition(Repetition::REQUIRED)
        .build()?;

    let generated_at = Type::primitive_type_builder("generated_at", PhysicalType::BYTE_ARRAY)
        .with_logical_type(Some(LogicalType::String))
        .with_repetition(Repetition::REQUIRED)
        .build()?;

    let begin_time = Type::primitive_type_builder("begin_time", PhysicalType::BYTE_ARRAY)
        .with_logical_type(Some(LogicalType::String))
        .with_repetition(Repetition::REQUIRED)
        .build()?;

    let end_time = Type::primitive_type_builder("end_time", PhysicalType::BYTE_ARRAY)
        .with_logical_type(Some(LogicalType::String))
        .with_repetition(Repetition::REQUIRED)
        .build()?;

    let max_temp = Type::primitive_type_builder("max_temp", PhysicalType::INT64)
        .with_repetition(Repetition::OPTIONAL)
        .build()?;

    let min_temp = Type::primitive_type_builder("min_temp", PhysicalType::INT64)
        .with_repetition(Repetition::OPTIONAL)
        .build()?;

    let temperature_unit_code =
        Type::primitive_type_builder("temperature_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()?;

    let wind_speed_value = Type::primitive_type_builder("wind_speed", PhysicalType::INT64)
        .with_repetition(Repetition::OPTIONAL)
        .build()?;

    let wind_speed_unit_code =
        Type::primitive_type_builder("wind_speed_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()?;

    let wind_direction_value = Type::primitive_type_builder("wind_direction", PhysicalType::INT64)
        .with_repetition(Repetition::OPTIONAL)
        .build()?;

    let wind_direction_unit_code =
        Type::primitive_type_builder("wind_direction_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()?;

    let relative_humidity_max =
        Type::primitive_type_builder("relative_humidity_max", PhysicalType::INT64)
            .with_repetition(Repetition::OPTIONAL)
            .build()?;

    let relative_humidity_min =
        Type::primitive_type_builder("relative_humidity_min", PhysicalType::INT64)
            .with_repetition(Repetition::OPTIONAL)
            .build()?;

    let relative_humidity_unit_code =
        Type::primitive_type_builder("relative_humidity_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()?;

    let liquid_precipitation_amt =
        Type::primitive_type_builder("liquid_precipitation_amt", PhysicalType::DOUBLE)
            .with_repetition(Repetition::OPTIONAL)
            .build()?;

    let liquid_precipitation_unit_code =
        Type::primitive_type_builder("liquid_precipitation_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()?;

    let twelve_hour_probability_of_precipitation = Type::primitive_type_builder(
        "twelve_hour_probability_of_precipitation",
        PhysicalType::INT64,
    )
    .with_repetition(Repetition::OPTIONAL)
    .build()?;

    let twelve_hour_probability_of_precipitation_unit_code = Type::primitive_type_builder(
        "twelve_hour_probability_of_precipitation_unit_code",
        PhysicalType::BYTE_ARRAY,
    )
    .with_logical_type(Some(LogicalType::String))
    .with_repetition(Repetition::REQUIRED)
    .build()?;

    // New fields at the end for backwards compatibility
    let state = Type::primitive_type_builder("state", PhysicalType::BYTE_ARRAY)
        .with_repetition(Repetition::REQUIRED)
        .with_logical_type(Some(LogicalType::String))
        .build()?;

    let iata_id = Type::primitive_type_builder("iata_id", PhysicalType::BYTE_ARRAY)
        .with_repetition(Repetition::REQUIRED)
        .with_logical_type(Some(LogicalType::String))
        .build()?;

    let elevation_m = Type::primitive_type_builder("elevation_m", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()?;

    let snow_amt = Type::primitive_type_builder("snow_amt", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()?;

    let snow_amt_unit_code =
        Type::primitive_type_builder("snow_amt_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()?;

    let snow_ratio = Type::primitive_type_builder("snow_ratio", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()?;

    let snow_ratio_unit_code =
        Type::primitive_type_builder("snow_ratio_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()?;

    let ice_amt = Type::primitive_type_builder("ice_amt", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()?;

    let ice_amt_unit_code =
        Type::primitive_type_builder("ice_amt_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()?;

    Type::group_type_builder("forecast")
        .with_fields(vec![
            Arc::new(station_id),
            Arc::new(station_name),
            Arc::new(latitude),
            Arc::new(longitude),
            Arc::new(generated_at),
            Arc::new(begin_time),
            Arc::new(end_time),
            Arc::new(max_temp),
            Arc::new(min_temp),
            Arc::new(temperature_unit_code),
            Arc::new(wind_speed_value),
            Arc::new(wind_speed_unit_code),
            Arc::new(wind_direction_value),
            Arc::new(wind_direction_unit_code),
            Arc::new(relative_humidity_max),
            Arc::new(relative_humidity_min),
            Arc::new(relative_humidity_unit_code),
            Arc::new(liquid_precipitation_amt),
            Arc::new(liquid_precipitation_unit_code),
            Arc::new(twelve_hour_probability_of_precipitation),
            Arc::new(twelve_hour_probability_of_precipitation_unit_code),
            // New fields at end
            Arc::new(state),
            Arc::new(iata_id),
            Arc::new(elevation_m),
            Arc::new(snow_amt),
            Arc::new(snow_amt_unit_code),
            Arc::new(snow_ratio),
            Arc::new(snow_ratio_unit_code),
            Arc::new(ice_amt),
            Arc::new(ice_amt_unit_code),
            optional_forecast_text("forecast_interval_version")?,
            optional_forecast_text("interval_kind")?,
            optional_forecast_text("source_url")?,
            optional_forecast_text("source_received_at")?,
            optional_forecast_text("source_xml_sha256")?,
            optional_forecast_text("source_location")?,
            optional_forecast_text("source_layouts")?,
            optional_forecast_text("quality_status")?,
            optional_forecast_text("quality_reason")?,
        ])
        .build()
}

fn optional_forecast_text(name: &str) -> Result<Arc<Type>, parquet::errors::ParquetError> {
    Type::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
        .with_logical_type(Some(LogicalType::String))
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .map(Arc::new)
}

#[derive(Debug, Clone)]
pub struct TimeRange {
    pub key: String,
    pub start_time: OffsetDateTime,
    pub end_time: Option<OffsetDateTime>,
}

/// Keep values on their own source intervals. Two metrics share a row only
/// when their native start and end instants are identical. A point sample has
/// begin_time == end_time; the reader must not invent a duration for it.
impl TryFrom<Dwml> for HashMap<String, Vec<WeatherForecast>> {
    type Error = anyhow::Error;
    fn try_from(raw_data: Dwml) -> Result<Self, Self::Error> {
        let generated_at = get_generated_at(&raw_data)
            .ok_or_else(|| anyhow!("forecast has no parseable creation-date"))?
            .to_offset(UtcOffset::UTC);
        let mut time_layouts = HashMap::new();
        for layout in &raw_data.data.time_layout {
            let ranges = layout.to_time_ranges()?;
            let key = ranges[0].key.clone();
            if time_layouts.insert(key.clone(), ranges).is_some() {
                return Err(anyhow!("duplicate forecast time layout {key:?}"));
            }
        }
        let mut locations = HashMap::new();
        for location in &raw_data.data.location {
            if locations
                .insert(location.location_key.as_str(), location)
                .is_some()
            {
                return Err(anyhow!(
                    "duplicate forecast location {:?}",
                    location.location_key
                ));
            }
        }
        let mut weather: HashMap<
            String,
            BTreeMap<(OffsetDateTime, OffsetDateTime), WeatherForecast>,
        > = HashMap::new();
        for parameters in &raw_data.data.parameters {
            let location = locations
                .get(parameters.applicable_location.as_str())
                .ok_or_else(|| {
                    anyhow!(
                        "parameters for unknown location {:?}",
                        parameters.applicable_location
                    )
                })?;
            let Some(station_id) = &location.station_id else {
                continue;
            };
            let station = weather.entry(station_id.clone()).or_default();
            let readings = parameters
                .temperature
                .iter()
                .flatten()
                .chain(parameters.humidity.iter().flatten())
                .chain(parameters.precipitation.iter().flatten())
                .chain(parameters.probability_of_precipitation.iter())
                .chain(parameters.wind_direction.iter())
                .chain(parameters.wind_speed.iter())
                .chain(parameters.winter_weather_outlook.iter());
            for reading in readings {
                let ranges = time_layouts.get(&reading.time_layout).ok_or_else(|| {
                    anyhow!("unknown forecast time layout {:?}", reading.time_layout)
                })?;
                if reading.value.len() != ranges.len() {
                    return Err(anyhow!(
                        "forecast {:?} has {} values for {} native intervals",
                        reading.reading_type,
                        reading.value.len(),
                        ranges.len()
                    ));
                }
                for (index, range) in ranges.iter().enumerate() {
                    let begin = range.start_time.to_offset(UtcOffset::UTC);
                    let end = range
                        .end_time
                        .unwrap_or(range.start_time)
                        .to_offset(UtcOffset::UTC);
                    let row = station
                        .entry((begin, end))
                        .or_insert_with(|| WeatherForecast {
                            station_id: station_id.clone(),
                            station_name: String::new(),
                            latitude: location.point.latitude.clone(),
                            longitude: location.point.longitude.clone(),
                            generated_at,
                            begin_time: begin,
                            end_time: end,
                            max_temp: None,
                            min_temp: None,
                            temperature_unit_code: Units::Fahrenheit.to_string(),
                            wind_speed: None,
                            wind_speed_unit_code: Units::Knots.to_string(),
                            wind_direction: None,
                            wind_direction_unit_code: Units::DegreesTrue.to_string(),
                            relative_humidity_max: None,
                            relative_humidity_min: None,
                            relative_humidity_unit_code: Units::Percent.to_string(),
                            liquid_precipitation_amt: None,
                            liquid_precipitation_unit_code: Units::Inches.to_string(),
                            snow_amt: None,
                            snow_amt_unit_code: Units::Inches.to_string(),
                            snow_ratio: None,
                            snow_ratio_unit_code: Units::Percent.to_string(),
                            ice_amt: None,
                            ice_amt_unit_code: Units::Inches.to_string(),
                            twelve_hour_probability_of_precipitation: None,
                            twelve_hour_probability_of_precipitation_unit_code: Units::Percent
                                .to_string(),
                            provenance: ForecastProvenance {
                                location: location.location_key.clone(),
                                ..Default::default()
                            },
                        });
                    add_native_value(row, range, reading, index)?;
                }
            }
        }
        Ok(weather
            .into_iter()
            .map(|(station, rows)| (station, rows.into_values().collect()))
            .collect())
    }
}

/// The source issue instant, never the collector's current time.
fn get_generated_at(raw_data: &Dwml) -> Option<OffsetDateTime> {
    raw_data
        .head
        .as_ref()
        .and_then(|head| head.product.as_ref())
        .and_then(|product| product.creation_date.as_ref())
        .and_then(|creation| OffsetDateTime::parse(&creation.value, &Rfc3339).ok())
}

fn metric_name(kind: &crate::Type) -> &'static str {
    match kind {
        Maximum => "max_temp",
        Minimum => "min_temp",
        Sustained => "wind_speed",
        Wind => "wind_direction",
        MaximumRelative => "relative_humidity_max",
        MinimumRelative => "relative_humidity_min",
        Liquid => "liquid_precipitation_amt",
        Snow => "snow_amt",
        Ice => "ice_amt",
        SnowRatio => "snow_ratio",
        ProbabilityOfPrecipitationWithin12Hours => "twelve_hour_probability_of_precipitation",
    }
}

fn add_native_value(
    row: &mut WeatherForecast,
    range: &TimeRange,
    data: &DataReading,
    index: usize,
) -> Result<(), Error> {
    let metric = metric_name(&data.reading_type);
    let raw = &data.value[index];
    let units = data.units.to_string();
    if row.provenance.layouts.contains_key(metric) {
        row.provenance
            .problems
            .push(format!("duplicate native {metric} for one interval"));
        return Ok(());
    }
    if matches!(data.reading_type, Maximum | Minimum)
        && row.provenance.layouts.iter().any(|(other, source)| {
            matches!(other.as_str(), "max_temp" | "min_temp") && source.units != units
        })
    {
        row.provenance
            .problems
            .push("mixed temperature units on the same native interval".into());
    }
    row.provenance.layouts.insert(
        metric.into(),
        NativeValueProvenance {
            layout: range.key.clone(),
            start: range.start_time.format(&Rfc3339)?,
            end: range.end_time.map(|end| end.format(&Rfc3339)).transpose()?,
            index,
            value: raw.clone(),
            units: units.clone(),
        },
    );
    if range.end_time.is_none() && !matches!(data.reading_type, Sustained | Wind) {
        row.provenance
            .problems
            .push(format!("{metric} has no explicit source interval"));
    }
    let valid_units = match data.reading_type {
        Maximum | Minimum => matches!(data.units, Units::Fahrenheit | Units::Celcius),
        Sustained => data.units == Units::Knots,
        Wind => data.units == Units::DegreesTrue,
        MaximumRelative | MinimumRelative | ProbabilityOfPrecipitationWithin12Hours | SnowRatio => {
            data.units == Units::Percent
        }
        Liquid | Snow | Ice => data.units == Units::Inches,
    };
    if !valid_units {
        row.provenance
            .problems
            .push(format!("unsupported {metric} units {units:?}"));
    }
    let (minimum, maximum) = match data.reading_type {
        Maximum | Minimum if data.units == Units::Fahrenheit => (-148.0, 140.0),
        Maximum | Minimum => (-100.0, 60.0),
        Wind => (0.0, 360.0),
        Sustained => (0.0, 250.0),
        MaximumRelative | MinimumRelative | ProbabilityOfPrecipitationWithin12Hours => (0.0, 100.0),
        SnowRatio => (0.0, 1000.0),
        Liquid | Snow | Ice => (0.0, f64::MAX),
    };
    let value = if raw.trim().is_empty() {
        None
    } else {
        match raw.trim().parse::<f64>() {
            Ok(value) if value.is_finite() && value >= minimum && value <= maximum => Some(value),
            _ => {
                row.provenance
                    .problems
                    .push(format!("invalid {metric} value {raw:?}"));
                None
            }
        }
    };
    let integer = if matches!(data.reading_type, Liquid | Snow | Ice | SnowRatio) {
        None
    } else {
        match value {
            Some(value) if value.fract() == 0.0 => Some(value as i64),
            Some(_) => {
                row.provenance
                    .problems
                    .push(format!("noninteger {metric} value {raw:?}"));
                None
            }
            None => None,
        }
    };
    match data.reading_type {
        Maximum => {
            row.max_temp = integer;
            row.temperature_unit_code = units;
        }
        Minimum => {
            row.min_temp = integer;
            row.temperature_unit_code = units;
        }
        Sustained => {
            row.wind_speed = integer;
            row.wind_speed_unit_code = units;
        }
        Wind => {
            row.wind_direction = integer;
            row.wind_direction_unit_code = units;
        }
        MaximumRelative => {
            row.relative_humidity_max = integer;
            row.relative_humidity_unit_code = units;
        }
        MinimumRelative => {
            row.relative_humidity_min = integer;
            row.relative_humidity_unit_code = units;
        }
        ProbabilityOfPrecipitationWithin12Hours => {
            row.twelve_hour_probability_of_precipitation = integer;
            row.twelve_hour_probability_of_precipitation_unit_code = units;
        }
        Liquid => {
            row.liquid_precipitation_amt = value;
            row.liquid_precipitation_unit_code = units;
        }
        Snow => {
            row.snow_amt = value;
            row.snow_amt_unit_code = units;
        }
        Ice => {
            row.ice_amt = value;
            row.ice_amt_unit_code = units;
        }
        SnowRatio => {
            row.snow_ratio = value;
            row.snow_ratio_unit_code = units;
        }
    }
    if row
        .min_temp
        .zip(row.max_temp)
        .is_some_and(|(minimum, maximum)| minimum > maximum)
    {
        row.provenance
            .problems
            .push("minimum temperature exceeds maximum on the same native interval".into());
    }
    if row
        .relative_humidity_min
        .zip(row.relative_humidity_max)
        .is_some_and(|(minimum, maximum)| minimum > maximum)
    {
        row.provenance
            .problems
            .push("minimum humidity exceeds maximum on the same native interval".into());
    }
    Ok(())
}

/// Stations per NDFD request.
const STATIONS_PER_REQUEST: usize = 50;
/// NDFD requests in flight at once.
const CONCURRENT_REQUESTS: usize = 4;
/// Attempts per request before its batch counts as failed.
const FETCH_ATTEMPTS: u32 = 3;

/// What one forecast run produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForecastReport {
    /// Stations requested from NOAA.
    pub expected_stations: usize,
    /// Stations with at least one forecast row written.
    pub written_stations: usize,
    pub rows: usize,
    pub failed_batches: usize,
}

impl ForecastReport {
    pub fn coverage(&self) -> f64 {
        if self.expected_stations == 0 {
            return 1.0;
        }
        self.written_stations as f64 / self.expected_stations as f64
    }
}

pub struct ForecastService {
    pub fetcher: Arc<XmlFetcher>,
    pub logger: Logger,
}

struct FetchedForecastBatch {
    rows: HashMap<String, Vec<WeatherForecast>>,
    source_document: KeyValue,
}

/// Keep the original response inside the published artifact. Compression
/// prevents repeated layout XML from dominating the Parquet footer; the hash
/// is over the original UTF-8 XML bytes, before compression.
async fn forecast_source_document(
    xml: &str,
    url: &str,
    received_at: &str,
    sha256: &str,
) -> Result<KeyValue, Error> {
    let mut encoder = GzipEncoder::new(Vec::new());
    encoder.write_all(xml.as_bytes()).await?;
    encoder.shutdown().await?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(encoder.into_inner());
    Ok(KeyValue::new(
        format!("noaa_forecast_source:{sha256}"),
        Some(serde_json::to_string(&serde_json::json!({
            "url": url, "received_at": received_at, "sha256": sha256,
            "encoding": "gzip+base64", "content": encoded,
        }))?),
    ))
}

impl ForecastService {
    pub fn new(logger: Logger, fetcher: Arc<XmlFetcher>) -> Self {
        ForecastService { logger, fetcher }
    }

    /// Fetches forecasts in batches of [`STATIONS_PER_REQUEST`] stations and
    /// writes each batch as a row group as it arrives. Every failure is
    /// counted in the report instead of being dropped silently; the caller
    /// decides whether coverage is good enough to publish.
    pub async fn get_forecasts_to_file(
        &self,
        city_weather: &CityWeather,
        output_path: &str,
    ) -> Result<ForecastReport, Error> {
        let (output, file) = PartialFile::create(output_path)
            .map_err(|e| anyhow!("failed to create parquet file: {}", e))?;
        let props = parquet_file::properties()
            .set_key_value_metadata(Some(vec![KeyValue::new(
                "noaa_forecast_interval_version".into(),
                Some(FORECAST_INTERVAL_VERSION.into()),
            )]))
            .build();
        let mut writer =
            SerializedFileWriter::new(file, Arc::new(create_forecast_schema()?), Arc::new(props))
                .map_err(|e| anyhow!("failed to create parquet writer: {}", e))?;

        let report = self
            .write_forecast_batches(city_weather, &mut writer)
            .await?;
        writer
            .close()
            .map_err(|e| anyhow!("failed to close parquet writer: {}", e))?;
        output
            .commit()
            .map_err(|e| anyhow!("failed to finish parquet file: {}", e))?;
        info!(
            self.logger,
            "forecasts: {} rows for {}/{} stations, {} failed batches, written to {}",
            report.rows,
            report.written_stations,
            report.expected_stations,
            report.failed_batches,
            output_path
        );
        Ok(report)
    }

    /// The fetch stream and station lookup end before the caller closes and
    /// publishes the Parquet writer, including when a batch write fails.
    async fn write_forecast_batches(
        &self,
        city_weather: &CityWeather,
        writer: &mut SerializedFileWriter<std::fs::File>,
    ) -> Result<ForecastReport, Error> {
        let batches = split_cityweather(city_weather.clone(), STATIONS_PER_REQUEST);
        let stations = &StationLookup::new(city_weather, &self.logger);
        let mut report = ForecastReport {
            expected_stations: city_weather.city_data.len(),
            written_stations: 0,
            rows: 0,
            failed_batches: 0,
        };
        let mut results = stream::iter(batches)
            .map(|batch| async move {
                let url = get_url(&batch)?;
                self.fetch_batch(&url, stations).await
            })
            .buffer_unordered(CONCURRENT_REQUESTS);
        while let Some(result) = results.next().await {
            let batch = match result {
                Ok(batch) => batch,
                Err(error) => {
                    report.failed_batches += 1;
                    error!(self.logger, "forecast batch failed: {:#}", error);
                    continue;
                }
            };
            writer.append_key_value_metadata(batch.source_document);
            let mut batch_forecasts = Vec::new();
            for (station_id, all_forecasts) in &batch.rows {
                let Some(city) = city_weather.city_data.get(station_id) else {
                    continue;
                };
                let before = batch_forecasts.len();
                for weather_forecast in all_forecasts {
                    match Forecast::try_from(weather_forecast.clone()) {
                        Ok(mut forecast) => {
                            forecast.station_name = city.station_name.clone();
                            forecast.state = city.state.clone();
                            forecast.iata_id = city.iata_id.clone();
                            forecast.elevation_m = city.elevation_m;
                            batch_forecasts.push(forecast);
                        }
                        Err(error) => {
                            error!(
                                self.logger,
                                "skipping forecast row for {}: {}", station_id, error
                            )
                        }
                    }
                }
                if batch_forecasts.len() > before {
                    report.written_stations += 1;
                }
            }
            if batch_forecasts.is_empty() {
                continue;
            }
            let mut row_group = writer
                .next_row_group()
                .map_err(|e| anyhow!("failed to create row group: {}", e))?;
            batch_forecasts
                .as_slice()
                .write_to_row_group(&mut row_group)
                .map_err(|e| anyhow!("failed to write row group: {}", e))?;
            row_group
                .close()
                .map_err(|e| anyhow!("failed to close row group: {}", e))?;
            report.rows += batch_forecasts.len();
        }
        Ok(report)
    }

    /// One NDFD request with bounded retries. An `<error>` document, a body
    /// that does not parse, or a conversion failure fails the batch.
    async fn fetch_batch(
        &self,
        url: &str,
        stations: &StationLookup,
    ) -> Result<FetchedForecastBatch, Error> {
        let mut delay = StdDuration::from_secs(5);
        let mut attempt = 1;
        let xml = loop {
            match self.fetcher.fetch_xml(url).await {
                Ok(xml) => break xml,
                Err(error) if attempt < FETCH_ATTEMPTS => {
                    error!(
                        self.logger,
                        "forecast request attempt {} failed, retrying in {:?}: {}",
                        attempt,
                        delay,
                        error
                    );
                    sleep(delay).await;
                    delay *= 2;
                    attempt += 1;
                }
                Err(error) => return Err(Error::new(error).context("forecast request failed")),
            }
        };
        let received = OffsetDateTime::now_utc();
        let received_at = received.format(&Rfc3339)?;
        let xml_sha256 = Sha256::digest(xml.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if xml.trim_start().starts_with("<error>") {
            return Err(anyhow!("NOAA returned an error document"));
        }
        let converted: Dwml = crate::parse_xml(&xml)
            .map_err(|error| anyhow!("forecast XML did not parse: {error}"))?;
        let mut rows: HashMap<String, Vec<WeatherForecast>> =
            stations.assign(converted).try_into()?;
        for station in rows.values_mut() {
            for row in station {
                row.provenance.source_url = Some(url.into());
                row.provenance.received_at = Some(received_at.clone());
                row.provenance.xml_sha256 = Some(xml_sha256.clone());
                if row.generated_at > received + Duration::minutes(10) {
                    row.provenance.problems.push(
                        "forecast issue is more than ten minutes after source receipt".into(),
                    );
                }
            }
        }
        Ok(FetchedForecastBatch {
            rows,
            source_document: forecast_source_document(&xml, url, &received_at, &xml_sha256).await?,
        })
    }
}

/// Maps NDFD point coordinates (2 decimal places) to station ids. When two
/// stations round to the same point the lowest id wins, so the result does
/// not depend on hash order; ambiguous points are logged once.
struct StationLookup {
    by_point: HashMap<(String, String), String>,
}

impl StationLookup {
    fn new(city_weather: &CityWeather, logger: &Logger) -> Self {
        let mut stations: Vec<&WeatherStation> = city_weather.city_data.values().collect();
        stations.sort_by(|a, b| a.station_id.cmp(&b.station_id));
        let mut by_point: HashMap<(String, String), String> = HashMap::new();
        let mut ambiguous = 0;
        for station in stations {
            let (Some(latitude), Some(longitude)) =
                (station.get_latitude(), station.get_longitude())
            else {
                continue;
            };
            match by_point.entry((latitude, longitude)) {
                std::collections::hash_map::Entry::Occupied(_) => ambiguous += 1,
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(station.station_id.clone());
                }
            }
        }
        if ambiguous > 0 {
            info!(
                logger,
                "{} stations share a forecast grid point with a lower station id and get no forecast",
                ambiguous
            );
        }
        Self { by_point }
    }

    fn assign(&self, mut converted_xml: Dwml) -> Dwml {
        for location in &mut converted_xml.data.location {
            location.station_id = self
                .by_point
                .get(&(
                    location.point.latitude.clone(),
                    location.point.longitude.clone(),
                ))
                .cloned();
        }
        converted_xml
    }
}

/// The NDFD request for a batch: the coming week.
fn get_url(city_weather: &CityWeather) -> Result<String, Error> {
    let (begin, end) = request_window(OffsetDateTime::now_utc())?;
    Ok(format!(
        "https://graphical.weather.gov/xml/sample_products/browser_interface/ndfdXMLclient.php?listLatLon={}&product=time-series&begin={begin}&end={end}&Unit=e&maxt=maxt&mint=mint&wspd=wspd&wdir=wdir&pop12=pop12&qpf=qpf&snow=snow&snowratio=snowratio&iceaccum=iceaccum&maxrh=maxrh&minrh=minrh",
        city_weather.get_coordinates_url(),
    ))
}

/// How far before the fetch a request's `begin` is written.
const REQUEST_LEAD: Duration = Duration::hours(6);

/// `begin` and `end` for a request made at `now`.
///
/// NDFD reads both as US Eastern local time; they carry no offset. Writing
/// them as UTC made each file's hourly values (wind) start four or five
/// hours after the fetch, so the newest file before an event never covered
/// its start and settlement could not form a wind baseline. `begin` is
/// written six hours before the fetch, on the hour: read as Eastern (UTC-4
/// or UTC-5) it falls one to two hours before the fetch all year, and read as
/// UTC it would only start earlier.
fn request_window(now: OffsetDateTime) -> Result<(String, String), Error> {
    let begin = (now - REQUEST_LEAD)
        .replace_minute(0)
        .and_then(|time| time.replace_second(0))
        .and_then(|time| time.replace_nanosecond(0))
        .map_err(|error| anyhow!("rounding the request time: {error}"))?;
    let format = format_description!(
        "[year]-[month padding:zero]-[day padding:zero]T[hour padding:zero]:[minute padding:zero]:[second padding:zero]"
    );
    Ok((
        begin.format(&format)?,
        (begin + Duration::weeks(1)).format(&format)?,
    ))
}

#[cfg(test)]
mod request_window_tests {
    use super::*;
    use time::{PrimitiveDateTime, UtcOffset, macros::datetime};

    /// The instant NDFD reads `text` as, at a US Eastern offset.
    fn as_eastern(text: &str, hours: i8) -> OffsetDateTime {
        let format = format_description!(
            "[year]-[month padding:zero]-[day padding:zero]T[hour padding:zero]:[minute padding:zero]:[second padding:zero]"
        );
        PrimitiveDateTime::parse(text, &format)
            .unwrap()
            .assume_offset(UtcOffset::from_hms(hours, 0, 0).unwrap())
    }

    #[test]
    fn hourly_values_start_before_the_fetch_in_either_eastern_offset() {
        for now in [
            datetime!(2026-09-27 22:31:47 UTC),
            datetime!(2026-01-15 00:05:00 UTC),
            datetime!(2026-03-08 06:59:59 UTC),
        ] {
            let (begin, end) = request_window(now).unwrap();
            for offset in [-4, -5] {
                let first = as_eastern(&begin, offset);
                assert!(
                    first <= now - Duration::hours(1),
                    "{now} {begin} at {offset}"
                );
                assert!(
                    first >= now - Duration::hours(3),
                    "{now} {begin} at {offset}"
                );
            }
            assert_eq!(
                as_eastern(&end, -4) - as_eastern(&begin, -4),
                Duration::weeks(1)
            );
        }
    }
}

#[cfg(test)]
#[path = "native_interval_tests.rs"]
mod native_interval_tests;
