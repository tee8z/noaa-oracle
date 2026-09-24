use crate::Type::{
    Ice, Liquid, Maximum, MaximumRelative, Minimum, MinimumRelative,
    ProbabilityOfPrecipitationWithin12Hours, Snow, SnowRatio, Sustained, Wind,
};
use crate::{CityWeather, DataReading, Dwml, Units, WeatherStation, XmlFetcher, split_cityweather};
use anyhow::{Error, anyhow};
use core::time::Duration as StdDuration;
use futures::stream::{self, StreamExt};
use parquet::basic::LogicalType;
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::record::RecordWriter;
use parquet::{
    basic::{Repetition, Type as PhysicalType},
    schema::types::Type,
};
use parquet_derive::ParquetRecordWriter;
use slog::{Logger, error, info};
use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;
use time::{
    Duration, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339,
    macros::format_description,
};
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
}

impl TryFrom<WeatherForecast> for Forecast {
    type Error = anyhow::Error;
    fn try_from(val: WeatherForecast) -> Result<Self, Self::Error> {
        let parquet = Forecast {
            station_id: val.station_id,
            station_name: String::from(""),
            latitude: val.latitude.parse::<f64>()?,
            longitude: val.longitude.parse::<f64>()?,
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
        };
        Ok(parquet)
    }
}

pub fn create_forecast_schema() -> Type {
    let station_id = Type::primitive_type_builder("station_id", PhysicalType::BYTE_ARRAY)
        .with_logical_type(Some(LogicalType::String))
        .with_repetition(Repetition::REQUIRED)
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

    let begin_time = Type::primitive_type_builder("begin_time", PhysicalType::BYTE_ARRAY)
        .with_logical_type(Some(LogicalType::String))
        .with_repetition(Repetition::REQUIRED)
        .build()
        .unwrap();

    let end_time = Type::primitive_type_builder("end_time", PhysicalType::BYTE_ARRAY)
        .with_logical_type(Some(LogicalType::String))
        .with_repetition(Repetition::REQUIRED)
        .build()
        .unwrap();

    let max_temp = Type::primitive_type_builder("max_temp", PhysicalType::INT64)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let min_temp = Type::primitive_type_builder("min_temp", PhysicalType::INT64)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let temperature_unit_code =
        Type::primitive_type_builder("temperature_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()
            .unwrap();

    let wind_speed_value = Type::primitive_type_builder("wind_speed", PhysicalType::INT64)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let wind_speed_unit_code =
        Type::primitive_type_builder("wind_speed_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()
            .unwrap();

    let wind_direction_value = Type::primitive_type_builder("wind_direction", PhysicalType::INT64)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let wind_direction_unit_code =
        Type::primitive_type_builder("wind_direction_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()
            .unwrap();

    let relative_humidity_max =
        Type::primitive_type_builder("relative_humidity_max", PhysicalType::INT64)
            .with_repetition(Repetition::OPTIONAL)
            .build()
            .unwrap();

    let relative_humidity_min =
        Type::primitive_type_builder("relative_humidity_min", PhysicalType::INT64)
            .with_repetition(Repetition::OPTIONAL)
            .build()
            .unwrap();

    let relative_humidity_unit_code =
        Type::primitive_type_builder("relative_humidity_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()
            .unwrap();

    let liquid_precipitation_amt =
        Type::primitive_type_builder("liquid_precipitation_amt", PhysicalType::DOUBLE)
            .with_repetition(Repetition::OPTIONAL)
            .build()
            .unwrap();

    let liquid_precipitation_unit_code =
        Type::primitive_type_builder("liquid_precipitation_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()
            .unwrap();

    let twelve_hour_probability_of_precipitation = Type::primitive_type_builder(
        "twelve_hour_probability_of_precipitation",
        PhysicalType::INT64,
    )
    .with_repetition(Repetition::OPTIONAL)
    .build()
    .unwrap();

    let twelve_hour_probability_of_precipitation_unit_code = Type::primitive_type_builder(
        "twelve_hour_probability_of_precipitation_unit_code",
        PhysicalType::BYTE_ARRAY,
    )
    .with_logical_type(Some(LogicalType::String))
    .with_repetition(Repetition::REQUIRED)
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

    let snow_amt = Type::primitive_type_builder("snow_amt", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let snow_amt_unit_code =
        Type::primitive_type_builder("snow_amt_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()
            .unwrap();

    let snow_ratio = Type::primitive_type_builder("snow_ratio", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let snow_ratio_unit_code =
        Type::primitive_type_builder("snow_ratio_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()
            .unwrap();

    let ice_amt = Type::primitive_type_builder("ice_amt", PhysicalType::DOUBLE)
        .with_repetition(Repetition::OPTIONAL)
        .build()
        .unwrap();

    let ice_amt_unit_code =
        Type::primitive_type_builder("ice_amt_unit_code", PhysicalType::BYTE_ARRAY)
            .with_logical_type(Some(LogicalType::String))
            .with_repetition(Repetition::REQUIRED)
            .build()
            .unwrap();

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
        ])
        .build()
        .unwrap()
}

#[derive(Debug, Clone)]
pub struct TimeRange {
    pub key: String,
    pub start_time: OffsetDateTime,
    pub end_time: Option<OffsetDateTime>,
}

//***THIS IS WHERE THE FLATTENING OF THE DATA OCCURS, IF THERE ARE ISSUES IN THE END DATA START HERE TO SOLVE***
impl TryFrom<Dwml> for HashMap<String, Vec<WeatherForecast>> {
    type Error = anyhow::Error;
    fn try_from(raw_data: Dwml) -> Result<Self, Self::Error> {
        let mut time_layouts: HashMap<String, Vec<TimeRange>> = HashMap::new();
        for time_layout in raw_data.data.time_layout.clone() {
            let time_range: Vec<TimeRange> = time_layout.to_time_ranges()?;
            if let Some(first) = time_range.first() {
                time_layouts.insert(first.key.clone(), time_range);
            }
        }

        let mut all_time_ranges: Vec<TimeRange> = Vec::new();
        for time_range_set in time_layouts.values() {
            for time_range in time_range_set {
                if let Some(end_time) = time_range.end_time {
                    // Compare as UTC instants to deduplicate cross-timezone duplicates
                    // (e.g., 07:00-06:00 CST and 08:00-05:00 EST are the same UTC instant)
                    let start_utc = time_range.start_time.to_offset(UtcOffset::UTC);
                    let end_utc = end_time.to_offset(UtcOffset::UTC);
                    if !all_time_ranges.iter().any(|existing| {
                        existing.start_time.to_offset(UtcOffset::UTC) == start_utc
                            && existing.end_time.map(|e| e.to_offset(UtcOffset::UTC))
                                == Some(end_utc)
                    }) {
                        all_time_ranges.push(time_range.clone());
                    }
                } else {
                    // For time ranges without end_time,
                    // we estimate end_time from the next time range

                    let estimated_end_time = estimate_end_time(time_range, time_range_set);
                    if let Some(end_time) = estimated_end_time {
                        let estimated_range = TimeRange {
                            key: time_range.key.clone(),
                            start_time: time_range.start_time,
                            end_time: Some(end_time),
                        };

                        let start_utc = estimated_range.start_time.to_offset(UtcOffset::UTC);
                        let end_utc = end_time.to_offset(UtcOffset::UTC);
                        if !all_time_ranges.iter().any(|existing| {
                            existing.start_time.to_offset(UtcOffset::UTC) == start_utc
                                && existing.end_time.map(|e| e.to_offset(UtcOffset::UTC))
                                    == Some(end_utc)
                        }) {
                            all_time_ranges.push(estimated_range);
                        }
                    }
                    // If we can't estimate, we skip this time range
                }
            }
        }

        // Sort by start time to ensure consistent ordering
        all_time_ranges.sort_by_key(|range| range.start_time);

        let generated_at = get_generated_at(&raw_data)
            .ok_or_else(|| anyhow!("forecast has no parseable creation-date"))?;

        // Create weather forecasts based on actual NOAA time ranges
        let mut weather: HashMap<String, Vec<WeatherForecast>> = HashMap::new();

        for location in &raw_data.data.location {
            let weather_forecasts: Vec<WeatherForecast> = all_time_ranges
                .iter()
                .filter_map(|time_range| Some((time_range, time_range.end_time?)))
                .map(|(time_range, end_time)| WeatherForecast {
                    station_id: location.station_id.clone().unwrap_or_default(),
                    station_name: String::from(""),
                    latitude: location.point.latitude.clone(),
                    longitude: location.point.longitude.clone(),
                    generated_at,
                    begin_time: time_range.start_time,
                    end_time,
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
                    twelve_hour_probability_of_precipitation_unit_code: Units::Percent.to_string(),
                })
                .collect();

            weather.insert(location.location_key.clone(), weather_forecasts);
        }

        for parameter_point in raw_data.data.parameters {
            let location_key = parameter_point.applicable_location.clone();
            let Some(weather_data) = weather.get_mut(&location_key) else {
                return Err(anyhow!("parameters for unknown location {location_key:?}"));
            };
            let layout = |key: &str| {
                time_layouts
                    .get(key)
                    .ok_or_else(|| anyhow!("unknown time layout {key:?}"))
            };
            let readings = parameter_point
                .temperature
                .into_iter()
                .flatten()
                .chain(parameter_point.humidity.into_iter().flatten())
                .chain(parameter_point.precipitation.into_iter().flatten())
                .chain(parameter_point.probability_of_precipitation)
                .chain(parameter_point.wind_direction)
                .chain(parameter_point.wind_speed)
                .chain(parameter_point.winter_weather_outlook);
            for reading in readings {
                add_data(weather_data, layout(&reading.time_layout)?, &reading);
            }
        }
        // The `station_id` is the key for each hashmap entry, if location doesn't have station_id, we skip
        let mut weather_by_station: HashMap<String, Vec<WeatherForecast>> = HashMap::new();
        raw_data.data.location.iter().for_each(|location| {
            if let Some(weather_forecast) = weather.get(&location.location_key)
                && let Some(station_id) = &location.station_id
            {
                let mut forecasts = weather_forecast.clone();
                for forecast in &mut forecasts {
                    forecast.station_id = station_id.clone();
                }
                weather_by_station.insert(station_id.clone(), forecasts);
            }
        });

        Ok(weather_by_station)
    }
}

/// When NOAA generated the forecast. Without it a batch cannot be ordered
/// against other forecasts for the same window, so it is rejected rather
/// than stamped with the current time.
fn get_generated_at(raw_data: &Dwml) -> Option<OffsetDateTime> {
    raw_data
        .head
        .as_ref()
        .and_then(|head| head.product.as_ref())
        .and_then(|product| product.creation_date.as_ref())
        .and_then(|creation_date| OffsetDateTime::parse(&creation_date.value, &Rfc3339).ok())
}

/// The reading's value for `index`, if the window matched and NOAA gave a
/// value. DWML marks missing values with `xsi:nil`, which deserialize as
/// empty strings and parse to `None`: missing, never zero or a previous
/// window's value.
fn value_at<T: std::str::FromStr>(data: &DataReading, index: Option<usize>) -> Option<T> {
    index
        .and_then(|index| data.value.get(index))
        .and_then(|value| value.trim().parse().ok())
}

/// Fills one reading into the 3-hour forecast windows. Instantaneous and
/// daily values (temperature, humidity, wind, probability) apply to every
/// window their NOAA range covers; accumulations (liquid, snow, ice, snow
/// ratio) apply only to the exactly matching window so a 12-hour total is
/// not counted four times. Windows the reading does not cover stay empty:
/// carrying a value forward would let yesterday's maximum leak into today.
fn add_data(weather_data: &mut [WeatherForecast], time_ranges: &[TimeRange], data: &DataReading) {
    let units = data.units.to_string();
    for current in weather_data.iter_mut() {
        let covering = get_interval(current, time_ranges);
        let exact = get_interval_exact(current, time_ranges);
        match data.reading_type {
            Liquid => {
                if exact.is_some() {
                    current.liquid_precipitation_amt = value_at(data, exact);
                }
                current.liquid_precipitation_unit_code = units.clone();
            }
            Snow => {
                if exact.is_some() {
                    current.snow_amt = value_at(data, exact);
                }
                current.snow_amt_unit_code = units.clone();
            }
            SnowRatio => {
                if exact.is_some() {
                    current.snow_ratio = value_at(data, exact);
                }
                current.snow_ratio_unit_code = units.clone();
            }
            Ice => {
                if exact.is_some() {
                    current.ice_amt = value_at(data, exact);
                }
                current.ice_amt_unit_code = units.clone();
            }
            Maximum => {
                if covering.is_some() {
                    current.max_temp = value_at(data, covering);
                }
                current.temperature_unit_code = units.clone();
            }
            Minimum => {
                if covering.is_some() {
                    current.min_temp = value_at(data, covering);
                }
                current.temperature_unit_code = units.clone();
            }
            MaximumRelative => {
                if covering.is_some() {
                    current.relative_humidity_max = value_at(data, covering);
                }
                current.relative_humidity_unit_code = units.clone();
            }
            MinimumRelative => {
                if covering.is_some() {
                    current.relative_humidity_min = value_at(data, covering);
                }
                current.relative_humidity_unit_code = units.clone();
            }
            Sustained => {
                if covering.is_some() {
                    current.wind_speed = value_at(data, covering);
                }
                current.wind_speed_unit_code = units.clone();
            }
            Wind => {
                if covering.is_some() {
                    current.wind_direction = value_at(data, covering);
                }
                current.wind_direction_unit_code = units.clone();
            }
            ProbabilityOfPrecipitationWithin12Hours => {
                if covering.is_some() {
                    current.twelve_hour_probability_of_precipitation = value_at(data, covering);
                }
                current.twelve_hour_probability_of_precipitation_unit_code = units.clone();
            }
        }
    }
}

fn estimate_end_time(
    current_range: &TimeRange,
    all_ranges: &[TimeRange],
) -> Option<OffsetDateTime> {
    // Find the next time range with the same key that starts after this one
    let next_range = all_ranges
        .iter()
        .filter(|r| r.key == current_range.key && r.start_time > current_range.start_time)
        .min_by_key(|r| r.start_time);

    if let Some(next) = next_range {
        // Use the next range's start time as this range's end time
        Some(next.start_time)
    } else {
        // If no next range found, estimate based on common intervals
        // Most NOAA forecasts are 1, 3, 6, 12, or 24 hours
        // We'll default to 3 hours as a reasonable estimate
        Some(current_range.start_time + Duration::hours(3))
    }
}

fn get_interval(current_data: &WeatherForecast, time_ranges: &[TimeRange]) -> Option<usize> {
    // First, try to find an exact match for the time range (when end_time is available)
    for (index, time_range) in time_ranges.iter().enumerate() {
        if let Some(end_time) = time_range.end_time
            && time_range.start_time == current_data.begin_time
            && end_time == current_data.end_time
        {
            return Some(index);
        }
    }

    // Try to find a match by start time only (for time ranges without end_time, like hourly wind data)
    for (index, time_range) in time_ranges.iter().enumerate() {
        if time_range.start_time == current_data.begin_time {
            return Some(index);
        }
    }

    // If no exact match, find the time range that contains this forecast's begin_time
    for (index, time_range) in time_ranges.iter().enumerate() {
        if let Some(end_time) = time_range.end_time
            && time_range.start_time <= current_data.begin_time
            && current_data.begin_time < end_time
        {
            return Some(index);
        }
    }

    // For time ranges without end_time, check if the forecast begin_time falls within
    // the implied interval (start_time to next start_time)
    for (index, time_range) in time_ranges.iter().enumerate() {
        if time_range.end_time.is_none() {
            // Find the next time range to determine the implied end time
            let next_start = time_ranges
                .get(index + 1)
                .map(|r| r.start_time)
                .unwrap_or(time_range.start_time + Duration::hours(3));

            if time_range.start_time <= current_data.begin_time
                && current_data.begin_time < next_start
            {
                return Some(index);
            }
        }
    }

    // If still no match, try to find overlap between time ranges
    for (index, time_range) in time_ranges.iter().enumerate() {
        if let Some(end_time) = time_range.end_time
            && ((time_range.start_time <= current_data.begin_time
                && current_data.begin_time < end_time)
                || (current_data.begin_time <= time_range.start_time
                    && time_range.start_time < current_data.end_time))
        {
            return Some(index);
        }
    }

    None
}

/// Strict interval matching for accumulative fields (QPF, snow, ice).
/// Only matches exact time range (begin+end) or exact start time.
/// Does NOT match sub-windows within larger NOAA ranges, preventing
/// the same accumulative value from being written to multiple overlapping windows.
fn get_interval_exact(current_data: &WeatherForecast, time_ranges: &[TimeRange]) -> Option<usize> {
    // Exact match: both begin and end times match
    for (index, time_range) in time_ranges.iter().enumerate() {
        if let Some(end_time) = time_range.end_time
            && time_range.start_time == current_data.begin_time
            && end_time == current_data.end_time
        {
            return Some(index);
        }
    }

    // Start time match only (for time ranges without end_time)
    for (index, time_range) in time_ranges.iter().enumerate() {
        if time_range.end_time.is_none() && time_range.start_time == current_data.begin_time {
            return Some(index);
        }
    }

    None
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
        let batches = split_cityweather(city_weather.clone(), STATIONS_PER_REQUEST);
        let file = File::create(output_path)
            .map_err(|e| anyhow!("failed to create parquet file: {}", e))?;
        let props = WriterProperties::builder().build();
        let mut writer =
            SerializedFileWriter::new(file, Arc::new(create_forecast_schema()), Arc::new(props))
                .map_err(|e| anyhow!("failed to create parquet writer: {}", e))?;

        let mut report = ForecastReport {
            expected_stations: city_weather.city_data.len(),
            written_stations: 0,
            rows: 0,
            failed_batches: 0,
        };
        let mut results = stream::iter(batches)
            .map(|batch| async move {
                let url = get_url(&batch)?;
                // Only this batch's stations: shared points may appear in other
                // batches, but each station must be written and counted once.
                let stations = StationLookup::new(&batch, &self.logger);
                self.fetch_batch(&url, &stations).await
            })
            .buffer_unordered(CONCURRENT_REQUESTS);
        while let Some(result) = results.next().await {
            let data = match result {
                Ok(data) => data,
                Err(error) => {
                    report.failed_batches += 1;
                    error!(self.logger, "forecast batch failed: {:#}", error);
                    continue;
                }
            };
            let mut batch_forecasts = Vec::new();
            for (station_id, all_forecasts) in &data {
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
        drop(results);
        writer
            .close()
            .map_err(|e| anyhow!("failed to close parquet writer: {}", e))?;
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

    /// One NDFD request with bounded retries. An `<error>` document, a body
    /// that does not parse, or a conversion failure fails the batch.
    async fn fetch_batch(
        &self,
        url: &str,
        stations: &StationLookup,
    ) -> Result<HashMap<String, Vec<WeatherForecast>>, Error> {
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
        if xml.trim_start().starts_with("<error>") {
            return Err(anyhow!("NOAA returned an error document"));
        }
        let converted: Dwml = crate::parse_xml(&xml)
            .map_err(|error| anyhow!("forecast XML did not parse: {error}"))?;
        stations.assign(converted).try_into()
    }
}

/// Maps NDFD point coordinates (2 decimal places) to all requested station
/// ids. Every station sharing a grid point receives that point's forecast.
struct StationLookup {
    by_point: HashMap<(String, String), Vec<String>>,
}

impl StationLookup {
    fn new(city_weather: &CityWeather, logger: &Logger) -> Self {
        let mut stations: Vec<&WeatherStation> = city_weather.city_data.values().collect();
        stations.sort_by(|a, b| a.station_id.cmp(&b.station_id));
        let mut by_point: HashMap<(String, String), Vec<String>> = HashMap::new();
        let mut ambiguous = 0;
        for station in stations {
            let (Some(latitude), Some(longitude)) =
                (station.get_latitude(), station.get_longitude())
            else {
                continue;
            };
            match by_point.entry((latitude, longitude)) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    ambiguous += 1;
                    entry.get_mut().push(station.station_id.clone());
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(vec![station.station_id.clone()]);
                }
            }
        }
        if ambiguous > 0 {
            info!(
                logger,
                "{} stations share another station's forecast grid point", ambiguous
            );
        }
        Self { by_point }
    }

    fn assign(&self, mut converted_xml: Dwml) -> Dwml {
        converted_xml.data.location = converted_xml
            .data
            .location
            .into_iter()
            .flat_map(|location| {
                match self.by_point.get(&(
                    location.point.latitude.clone(),
                    location.point.longitude.clone(),
                )) {
                    Some(stations) => stations
                        .iter()
                        .map(|station_id| {
                            let mut assigned = location.clone();
                            assigned.station_id = Some(station_id.clone());
                            assigned
                        })
                        .collect::<Vec<_>>(),
                    // Keep unknown points so their parameter blocks still parse;
                    // conversion omits them from the station output.
                    None => vec![location],
                }
            })
            .collect();
        converted_xml
    }
}

/// The NDFD request for a batch: the coming week, starting at the nearest
/// hour.
fn get_url(city_weather: &CityWeather) -> Result<String, Error> {
    let now = OffsetDateTime::now_utc() + Duration::minutes(30);
    let begin = now
        .replace_minute(0)
        .and_then(|time| time.replace_second(0))
        .and_then(|time| time.replace_nanosecond(0))
        .map_err(|error| anyhow!("rounding the request time: {error}"))?;
    let format = format_description!(
        "[year]-[month padding:zero]-[day padding:zero]T[hour padding:zero]:[minute padding:zero]:[second padding:zero]"
    );
    Ok(format!(
        "https://graphical.weather.gov/xml/sample_products/browser_interface/ndfdXMLclient.php?listLatLon={}&product=time-series&begin={}&end={}&Unit=e&maxt=maxt&mint=mint&wspd=wspd&wdir=wdir&pop12=pop12&qpf=qpf&snow=snow&snowratio=snowratio&iceaccum=iceaccum&maxrh=maxrh&minrh=minrh",
        city_weather.get_coordinates_url(),
        begin.format(&format)?,
        (begin + Duration::weeks(1)).format(&format)?,
    ))
}

#[cfg(test)]
mod shared_point_tests {
    use super::*;

    #[test]
    fn every_station_at_a_shared_point_receives_its_own_forecast_rows() {
        let city_weather = CityWeather {
            city_data: ["KAAA", "KORD"]
                .into_iter()
                .map(|id| {
                    (
                        id.to_string(),
                        WeatherStation {
                            station_id: id.into(),
                            station_name: id.into(),
                            state: "IL".into(),
                            iata_id: String::new(),
                            elevation_m: None,
                            latitude: "41.98".into(),
                            longitude: "-87.90".into(),
                        },
                    )
                })
                .collect(),
        };
        let logger = Logger::root(slog::Discard, slog::o!());
        let lookup = StationLookup::new(&city_weather, &logger);
        let raw: Dwml = crate::parse_xml(include_str!("testdata/dwml.xml")).unwrap();
        let weather: HashMap<String, Vec<WeatherForecast>> = lookup.assign(raw).try_into().unwrap();
        assert_eq!(weather.len(), 2);
        assert!(!weather["KAAA"].is_empty());
        assert_eq!(weather["KAAA"].len(), weather["KORD"].len());
        for (left, right) in weather["KAAA"].iter().zip(&weather["KORD"]) {
            assert_eq!(left.station_id, "KAAA");
            assert_eq!(right.station_id, "KORD");
            assert_eq!(left.begin_time, right.begin_time);
            assert_eq!(left.end_time, right.end_time);
            assert_eq!(left.max_temp, right.max_temp);
            assert_eq!(
                left.liquid_precipitation_amt,
                right.liquid_precipitation_amt
            );
        }
    }
}
