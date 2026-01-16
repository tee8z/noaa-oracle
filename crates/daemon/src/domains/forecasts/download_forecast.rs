use crate::Type::{
    Liquid, Maximum, MaximumRelative, Minimum, MinimumRelative,
    ProbabilityOfPrecipitationWithin12Hours, Sustained, Wind,
};
use crate::{
    split_cityweather, CityWeather, DataReading, Dwml, Location, Units, WeatherStation, XmlFetcher,
};
use anyhow::{anyhow, Error};
use core::time::Duration as StdDuration;
use parquet::basic::LogicalType;
use parquet::{
    basic::{Repetition, Type as PhysicalType},
    schema::types::Type,
};
use parquet_derive::ParquetRecordWriter;
use serde_xml_rs::from_str;
use slog::{debug, error, info, Logger};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::{collections::HashMap, ops::Add};
use time::{
    format_description::well_known::Rfc3339, macros::format_description, Duration, OffsetDateTime,
};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
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

    let schema = Type::group_type_builder("forecast")
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
        ])
        .build()
        .unwrap();

    schema
}

#[derive(Debug, Clone)]
pub struct TimeDelta {
    pub first_start: OffsetDateTime,
    pub last_end: OffsetDateTime,
    pub delta_between_readings: Duration,
    pub delta_between_start_and_end: Option<Duration>,
    pub key: String,
    pub time_ranges: Vec<TimeRange>,
}

#[derive(Debug, Clone)]
pub struct TimeRange {
    pub key: String,
    pub start_time: OffsetDateTime,
    pub end_time: Option<OffsetDateTime>,
}

#[derive(Debug, Clone)]
pub struct TimeWindow {
    pub first_time: OffsetDateTime,
    pub last_time: OffsetDateTime,
    pub time_interval: Duration,
}

//***THIS IS WHERE THE FLATTENING OF THE DATA OCCURS, IF THERE ARE ISSUES IN THE END DATA START HERE TO SOLVE***
impl TryFrom<Dwml> for HashMap<String, Vec<WeatherForecast>> {
    type Error = anyhow::Error;
    fn try_from(raw_data: Dwml) -> Result<Self, Self::Error> {
        let mut time_layouts: HashMap<String, Vec<TimeRange>> = HashMap::new();
        for time_layout in raw_data.data.time_layout.clone() {
            let time_range: Vec<TimeRange> = time_layout.to_time_ranges()?;
            time_layouts.insert(time_range.first().unwrap().key.clone(), time_range);
        }

        let mut all_time_ranges: Vec<TimeRange> = Vec::new();
        for time_range_set in time_layouts.values() {
            for time_range in time_range_set {
                if let Some(end_time) = time_range.end_time {
                    if !all_time_ranges.iter().any(|existing| {
                        existing.start_time == time_range.start_time
                            && existing.end_time == Some(end_time)
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

                        if !all_time_ranges.iter().any(|existing| {
                            existing.start_time == estimated_range.start_time
                                && existing.end_time == Some(end_time)
                        }) {
                            all_time_ranges.push(estimated_range);
                        }
                    }
                    // If we can't estimate, we skip this time range
                }
            }
        }

        // Sort by start time to ensure consistent ordering
        all_time_ranges.sort_by(|a, b| a.start_time.cmp(&b.start_time));

        let generated_at = get_generated_at(&raw_data);

        // Create weather forecasts based on actual NOAA time ranges
        let mut weather: HashMap<String, Vec<WeatherForecast>> = HashMap::new();

        for location in &raw_data.data.location {
            let weather_forecasts: Vec<WeatherForecast> = all_time_ranges
                .iter()
                .map(|time_range| {
                    WeatherForecast {
                        station_id: location.station_id.clone().unwrap_or_default(),
                        station_name: String::from(""),
                        latitude: location.point.latitude.clone(),
                        longitude: location.point.longitude.clone(),
                        generated_at,
                        begin_time: time_range.start_time,
                        end_time: time_range.end_time.unwrap(), // Safe because we filtered out None values
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
                        twelve_hour_probability_of_precipitation: None,
                        twelve_hour_probability_of_precipitation_unit_code: Units::Percent
                            .to_string(),
                    }
                })
                .collect();

            weather.insert(location.location_key.clone(), weather_forecasts);
        }

        // Used to pull the data forward from last time we had a forecast for a value
        let mut prev_weather = weather.clone();
        for parameter_point in raw_data.data.parameters {
            let location_key = parameter_point.applicable_location.clone();
            let weather_data = weather.get_mut(&location_key).unwrap();
            let prev_forecast_val: &mut WeatherForecast = prev_weather
                .get_mut(&location_key)
                .unwrap()
                .first_mut()
                .unwrap();

            if let Some(temps) = parameter_point.temperature {
                for temp in temps {
                    // We want this to panic, we should never have a time layout that doesn't exist in the map
                    let temp_times = time_layouts.get(&temp.time_layout).unwrap();
                    add_data(weather_data, temp_times, &temp, prev_forecast_val)?;
                }
            }

            if let Some(humidities) = parameter_point.humidity {
                for humidity in humidities {
                    let humidity_times = time_layouts.get(&humidity.time_layout).unwrap();
                    add_data(weather_data, humidity_times, &humidity, prev_forecast_val)?;
                }
            }

            if let Some(precipitation) = parameter_point.precipitation {
                let precipitation_times = time_layouts.get(&precipitation.time_layout).unwrap();
                add_data(
                    weather_data,
                    precipitation_times,
                    &precipitation,
                    prev_forecast_val,
                )?;
            }

            if let Some(probability_of_precipitation) = parameter_point.probability_of_precipitation
            {
                let probability_of_precipitation_times = time_layouts
                    .get(&probability_of_precipitation.time_layout)
                    .unwrap();
                add_data(
                    weather_data,
                    probability_of_precipitation_times,
                    &probability_of_precipitation,
                    prev_forecast_val,
                )?;
            }

            if let Some(wind_direction) = parameter_point.wind_direction {
                let wind_direction_times = time_layouts.get(&wind_direction.time_layout).unwrap();
                add_data(
                    weather_data,
                    wind_direction_times,
                    &wind_direction,
                    prev_forecast_val,
                )?;
            }

            if let Some(wind_speed) = parameter_point.wind_speed {
                let wind_speed_times = time_layouts.get(&wind_speed.time_layout).unwrap();
                add_data(
                    weather_data,
                    wind_speed_times,
                    &wind_speed,
                    prev_forecast_val,
                )?;
            }
        }
        // The `station_id` is the key for each hashmap entry, if location doesn't have station_id, we skip
        let mut weather_by_station: HashMap<String, Vec<WeatherForecast>> = HashMap::new();
        raw_data.data.location.iter().for_each(|location| {
            if let Some(weather_forecast) = weather.get(&location.location_key) {
                if let Some(station_id) = &location.station_id {
                    weather_by_station.insert(station_id.clone(), weather_forecast.clone());
                }
            }
        });

        Ok(weather_by_station)
    }
}

fn get_generated_at(raw_data: &Dwml) -> OffsetDateTime {
    if let Some(head) = raw_data.head.clone() {
        if let Some(product) = head.product {
            if let Some(creation_date) = product.creation_date {
                return match OffsetDateTime::parse(&creation_date, &Rfc3339) {
                    Ok(time) => time,
                    Err(_) => OffsetDateTime::now_utc(),
                };
            }
        }
    }
    OffsetDateTime::now_utc()
}

// weather_data is always in 3 hour intervals, time_ranges can be in 3,6,12,24 ranges
fn add_data(
    weather_data: &mut [WeatherForecast],
    time_ranges: &[TimeRange],
    data: &DataReading,
    prev_weather_data: &mut WeatherForecast,
) -> Result<(), Error> {
    for current_data in weather_data.iter_mut() {
        let time_interval_index = get_interval(current_data, time_ranges);

        match data.reading_type {
            Liquid => {
                if let Some(index) = time_interval_index {
                    current_data.liquid_precipitation_amt = data
                        .value
                        .get(index)
                        .and_then(|value| value.parse::<f64>().ok())
                        .map_or(prev_weather_data.liquid_precipitation_amt, |parsed_value| {
                            prev_weather_data.liquid_precipitation_amt = Some(parsed_value);
                            Some(parsed_value)
                        });
                } else {
                    current_data.liquid_precipitation_amt =
                        prev_weather_data.liquid_precipitation_amt;
                }
                current_data.liquid_precipitation_unit_code = data.units.to_string();
            }
            Maximum => {
                if let Some(index) = time_interval_index {
                    current_data.max_temp = data
                        .value
                        .get(index)
                        .and_then(|value| value.parse::<i64>().ok())
                        .map_or(prev_weather_data.max_temp, |parsed_value| {
                            prev_weather_data.max_temp = Some(parsed_value);
                            Some(parsed_value)
                        });
                } else {
                    current_data.max_temp = prev_weather_data.max_temp;
                }
                current_data.temperature_unit_code = data.units.to_string();
            }
            Minimum => {
                if let Some(index) = time_interval_index {
                    current_data.min_temp = data
                        .value
                        .get(index)
                        .and_then(|value: &String| value.parse::<i64>().ok())
                        .map_or(prev_weather_data.min_temp, |parsed_value| {
                            prev_weather_data.min_temp = Some(parsed_value);
                            Some(parsed_value)
                        });
                } else {
                    current_data.min_temp = prev_weather_data.min_temp;
                }
                current_data.temperature_unit_code = data.units.to_string();
            }
            MaximumRelative => {
                if let Some(index) = time_interval_index {
                    current_data.relative_humidity_max = data
                        .value
                        .get(index)
                        .and_then(|value: &String| value.parse::<i64>().ok())
                        .map_or(prev_weather_data.relative_humidity_max, |parsed_value| {
                            prev_weather_data.relative_humidity_max = Some(parsed_value);
                            Some(parsed_value)
                        });
                } else {
                    current_data.relative_humidity_max = prev_weather_data.relative_humidity_max;
                }
                current_data.relative_humidity_unit_code = data.units.to_string();
            }
            MinimumRelative => {
                if let Some(index) = time_interval_index {
                    current_data.relative_humidity_min = data
                        .value
                        .get(index)
                        .and_then(|value: &String| value.parse::<i64>().ok())
                        .map_or(prev_weather_data.relative_humidity_min, |parsed_value| {
                            prev_weather_data.relative_humidity_min = Some(parsed_value);
                            Some(parsed_value)
                        });
                } else {
                    current_data.relative_humidity_min = prev_weather_data.relative_humidity_min;
                }
                current_data.relative_humidity_unit_code = data.units.to_string();
            }
            Sustained => {
                if let Some(index) = time_interval_index {
                    current_data.wind_speed = data
                        .value
                        .get(index)
                        .and_then(|value: &String| value.parse::<i64>().ok())
                        .map_or(prev_weather_data.wind_speed, |parsed_value| {
                            prev_weather_data.wind_speed = Some(parsed_value);
                            Some(parsed_value)
                        });
                } else {
                    current_data.wind_speed = prev_weather_data.wind_speed;
                }
                current_data.wind_speed_unit_code = data.units.to_string();
            }
            ProbabilityOfPrecipitationWithin12Hours => {
                if let Some(index) = time_interval_index {
                    current_data.twelve_hour_probability_of_precipitation = data
                        .value
                        .get(index)
                        .and_then(|value: &String| value.parse::<i64>().ok())
                        .map_or(
                            prev_weather_data.twelve_hour_probability_of_precipitation,
                            |parsed_value| {
                                prev_weather_data.twelve_hour_probability_of_precipitation =
                                    Some(parsed_value);
                                Some(parsed_value)
                            },
                        );
                } else {
                    current_data.twelve_hour_probability_of_precipitation =
                        prev_weather_data.twelve_hour_probability_of_precipitation;
                }
                current_data.twelve_hour_probability_of_precipitation_unit_code =
                    data.units.to_string();
            }
            Wind => {
                if let Some(index) = time_interval_index {
                    current_data.wind_direction = data
                        .value
                        .get(index)
                        .and_then(|value: &String| value.parse::<i64>().ok())
                        .map_or(prev_weather_data.wind_direction, |parsed_value| {
                            prev_weather_data.wind_direction = Some(parsed_value);
                            Some(parsed_value)
                        });
                } else {
                    current_data.wind_direction = prev_weather_data.wind_direction;
                }
                current_data.wind_direction_unit_code = data.units.to_string();
            }
        }
    }
    Ok(())
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
    // First, try to find an exact match for the time range
    for (index, time_range) in time_ranges.iter().enumerate() {
        if let Some(end_time) = time_range.end_time {
            if time_range.start_time == current_data.begin_time && end_time == current_data.end_time
            {
                return Some(index);
            }
        }
    }

    // If no exact match, find the time range that contains this forecast's begin_time
    for (index, time_range) in time_ranges.iter().enumerate() {
        if let Some(end_time) = time_range.end_time {
            if time_range.start_time <= current_data.begin_time
                && current_data.begin_time < end_time
            {
                return Some(index);
            }
        }
    }

    // If still no match, try to find overlap between time ranges
    for (index, time_range) in time_ranges.iter().enumerate() {
        if let Some(end_time) = time_range.end_time {
            if (time_range.start_time <= current_data.begin_time
                && current_data.begin_time < end_time)
                || (current_data.begin_time <= time_range.start_time
                    && time_range.start_time < current_data.end_time)
            {
                return Some(index);
            }
        }
    }

    None
}

pub struct ForecastRetry {
    pub tx: mpsc::Sender<Result<HashMap<String, Vec<WeatherForecast>>, Error>>,
    pub max_retries: usize,
    pub fetcher: Arc<XmlFetcher>,
    pub logger: Logger,
}

impl ForecastRetry {
    pub fn new(
        tx: mpsc::Sender<Result<HashMap<String, Vec<WeatherForecast>>, Error>>,
        max_retries: usize,
        fetcher: Arc<XmlFetcher>,
        logger: Logger,
    ) -> Self {
        ForecastRetry {
            tx,
            max_retries,
            fetcher,
            logger,
        }
    }

    pub async fn fetch_forecast_with_retry(
        &self,
        url: String,
        city_weather: &CityWeather,
    ) -> Result<(), Error> {
        info!(self.logger, "url: {}", url);
        loop {
            match self.fetcher.fetch_xml(&url).await {
                Ok(xml) => {
                    let converted_xml: Dwml = match from_str(&xml) {
                        Ok(xml) => xml,
                        Err(err) => {
                            error!(
                                self.logger,
                                "error converting xml: {} \n raw string: {}", err, xml
                            );
                            Dwml::default()
                        }
                    };
                    if converted_xml == Dwml::default() {
                        info!(
                            self.logger,
                            "no current forecast xml found, skipping converting"
                        );
                        if let Err(err) = self.tx.send(Ok(HashMap::new())).await {
                            error!(self.logger, "Error sending result through channel: {}", err);
                            return Ok(());
                        }
                        return Ok(());
                    }
                    let weather_with_stations = add_station_ids(city_weather, converted_xml);
                    let current_forecast_data: HashMap<String, Vec<WeatherForecast>> =
                        match weather_with_stations.try_into() {
                            Ok(weather) => weather,
                            Err(err) => {
                                error!(self.logger, "error converting to Forecast: {}", err);

                                HashMap::new()
                            }
                        };
                    if current_forecast_data.is_empty() {
                        info!(self.logger, "no current forecast data found");
                        return Ok(());
                    }
                    // Send the result through the channel
                    if let Err(err) = self.tx.send(Ok(current_forecast_data)).await {
                        error!(self.logger, "Error sending result through channel: {}", err);
                    }

                    return Ok(());
                }
                Err(err) => {
                    // Log the error and retry after a delay
                    error!(self.logger, "Error fetching XML: {}", err);
                    sleep(StdDuration::from_secs(5)).await;
                }
            }
        }
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
    pub async fn get_forecasts(&self, city_weather: &CityWeather) -> Result<Vec<Forecast>, Error> {
        let split_maps = split_cityweather(city_weather.clone(), 50);
        let total_requests = split_maps.len();
        let (tx, mut rx) =
            mpsc::channel::<Result<HashMap<String, Vec<WeatherForecast>>, Error>>(total_requests);

        let max_retries = 3;
        let request_counter = Arc::new(AtomicUsize::new(total_requests));
        let mut set = JoinSet::new();
        for city_weather in split_maps {
            let url = get_url(&city_weather);
            let counter_clone = Arc::clone(&request_counter);
            let forecast_retry = ForecastRetry::new(
                tx.clone(),
                max_retries,
                self.fetcher.clone(),
                self.logger.clone(),
            );
            let logger_cpy = self.logger.clone();

            set.spawn(async move {
                match forecast_retry
                    .fetch_forecast_with_retry(url.clone(), &city_weather)
                    .await
                {
                    Ok(_) => {
                        info!(&logger_cpy, "completed getting forecast data for: {}", url);
                        counter_clone.fetch_sub(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        error!(&logger_cpy, "error getting forecast data for: {}", url);
                        counter_clone.fetch_sub(1, Ordering::Relaxed);
                    }
                }
            });
        }

        let forecast_data = Arc::new(Mutex::new(HashMap::new()));
        let forecast_data_clone = Arc::clone(&forecast_data);
        let logger_clone = self.logger.clone();
        set.spawn(async move {
            while let Some(result) = rx.recv().await {
                match result {
                    Ok(data) => {
                        info!(
                            &logger_clone,
                            "found more forecast data for: {:?}",
                            data.keys()
                        );
                        let mut forecast_data = forecast_data_clone.lock().await;
                        //using station_id as the key
                        forecast_data.extend(data);
                    }
                    Err(err) => {
                        error!(&logger_clone, "Error fetching forecast data: {}", err);
                    }
                }

                let batches_left = request_counter.load(Ordering::Relaxed);
                if batches_left > 0 {
                    let progress = ((total_requests as f64 - batches_left as f64)
                        / total_requests as f64)
                        * 100_f64;
                    info!(
                        &logger_clone,
                        "waiting for next batch of weather data, batches left: {} progress: {:.2}%",
                        batches_left,
                        progress
                    );
                } else {
                    rx.close();
                    rx.recv().await;
                    info!(&logger_clone, "all request have completed, moving on");
                    break;
                }
            }
        });

        while let Some(inner_res) = set.join_next().await {
            match inner_res {
                Ok(_) => info!(self.logger, "task finished"),
                Err(e) => error!(self.logger, "error with task: {}", e),
            }
        }

        info!(self.logger, "done waiting for data, continuing");
        let mut forecasts = vec![];
        for all_forecasts in forecast_data.lock().await.values() {
            for weather_forecats in all_forecasts {
                let current = weather_forecats.clone();
                debug!(
                    self.logger.clone(),
                    "current weather forecast: {:?}", current
                );
                let mut forecast: Forecast = current.try_into()?;
                debug!(
                    self.logger.clone(),
                    "parquet format forecast: {:?}", forecast
                );
                let city = city_weather.city_data.get(&forecast.station_id).unwrap();
                forecast.station_name = city.station_name.clone();
                forecast.state = city.state.clone();
                forecast.iata_id = city.iata_id.clone();
                forecast.elevation_m = city.elevation_m;
                forecasts.push(forecast)
            }
        }

        Ok(forecasts)
    }
}

fn add_station_ids(city_weather: &CityWeather, mut converted_xml: Dwml) -> Dwml {
    converted_xml.data.location = converted_xml
        .data
        .location
        .iter()
        .map(|location| {
            let latitude = location.point.latitude.clone();
            let longitude = location.point.longitude.clone();

            let station_id = city_weather
                .city_data
                .clone()
                .values()
                .find(|val| compare_coordinates(val, &latitude, &longitude))
                .map(|val| val.station_id.clone());

            Location {
                location_key: location.location_key.clone(),
                point: location.point.clone(),
                station_id,
            }
        })
        .collect();
    converted_xml
}

// forecast xml files always provide these to 2 decimal places, make sure to match on that percision
fn compare_coordinates(weather_station: &WeatherStation, latitude: &str, longitude: &str) -> bool {
    let station_lat = weather_station.get_latitude();
    let station_long = weather_station.get_longitude();

    station_lat == latitude && station_long == longitude
}

fn get_url(city_weather: &CityWeather) -> String {
    // Get the current time
    let mut current_time = OffsetDateTime::now_utc();

    // Round to the nearest hour
    if current_time.minute() > 30 {
        let hour = if current_time.hour() == 23 {
            0
        } else {
            current_time.hour() + 1
        };
        current_time = current_time
            .replace_hour(hour)
            .unwrap()
            .replace_minute(0)
            .unwrap()
            .replace_second(0)
            .unwrap();
    } else {
        current_time = current_time
            .replace_minute(0)
            .unwrap()
            .replace_second(0)
            .unwrap();
    }

    // Format the rounded current time
    let format_description = format_description!("[year]-[month padding:zero]-[day padding:zero]T[hour padding:zero]:[minute padding:zero]:[second padding:zero]");
    let now = current_time.format(&format_description).unwrap();

    // Define the duration of one week (7 days)
    let one_week_duration = Duration::weeks(1);
    let one_week_from_now = current_time.add(one_week_duration);

    let one_week = one_week_from_now.format(&format_description).unwrap();
    format!("https://graphical.weather.gov/xml/sample_products/browser_interface/ndfdXMLclient.php?listLatLon={}&product=time-series&begin={}&end={}&Unit=e&maxt=maxt&mint=mint&wspd=wspd&wdir=wdir&pop12=pop12&qpf=qpf&maxrh=maxrh&minrh=minrh", city_weather.get_coordinates_url(),now,one_week)
}
