use anyhow::{Error, anyhow};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::record::RecordWriter;
use parquet::{
    basic::{LogicalType, Repetition, Type as PhysicalType},
    schema::types::Type,
};
use parquet_derive::ParquetRecordWriter;
use slog::{Logger, info, warn};
use std::fs::File;
use std::sync::Arc;
use time::{OffsetDateTime, format_description::well_known::Rfc3339, macros::format_description};

use crate::{CityWeather, Metar, ObservationData, Units, XmlFetcher, parse_xml};

#[derive(Clone)]
pub struct CurrentWeather {
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
        Ok(CurrentWeather {
            station_id: val.station_id.clone(),
            latitude: val.latitude.unwrap_or(String::from("")).parse::<f64>()?,
            longitude: val.longitude.unwrap_or(String::from("")).parse::<f64>()?,
            generated_at: OffsetDateTime::parse(
                val.observation_time
                    .as_deref()
                    .ok_or_else(|| anyhow!("report has no observation_time"))?,
                &Rfc3339,
            )
            .map_err(|e| {
                anyhow!(
                    "error parsing observation_time time: {} {:?}",
                    e,
                    val.observation_time
                )
            })?,
            temperature_value: val
                .temp_c
                .unwrap_or(String::from(""))
                .parse::<f64>()
                .map(Some)
                .unwrap_or(None),
            temperature_unit_code: Units::Celcius.to_string(),
            wind_direction: val
                .wind_dir_degrees
                .unwrap_or(String::from(""))
                .parse::<i64>()
                .map(Some)
                .unwrap_or(None),
            wind_direction_unit_code: Units::DegreesTrue.to_string(),
            wind_speed: val
                .wind_speed_kt
                .unwrap_or(String::from(""))
                .parse::<i64>()
                .map(Some)
                .unwrap_or(None),
            wind_speed_unit_code: Units::Knots.to_string(),
            dewpoint_value: val
                .dewpoint_c
                .unwrap_or(String::from(""))
                .parse::<f64>()
                .map(Some)
                .unwrap_or(None),
            dewpoint_unit_code: Units::Celcius.to_string(),
            precip_in: precipitation(val.precip_in.as_deref(), &val.raw_text),
            precip_unit_code: Units::Inches.to_string(),
            wx_string: val.wx_string.unwrap_or_default(),
        })
    }
}

/// Hourly precipitation in inches. At AO2 automated stations the METAR
/// precipitation group is omitted when none fell, so absence means 0.
/// Elsewhere (AO1 and manual stations have no precipitation sensor)
/// absence means unknown. `VRB` and absent wind directions likewise stay
/// null rather than becoming 0 (north).
fn precipitation(precip_in: Option<&str>, raw_text: &str) -> Option<f64> {
    match precip_in.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.parse::<f64>().ok(),
        None => is_ao2(raw_text).then_some(0.0),
    }
}

/// Whether the report's remarks declare an AO2 station (precipitation
/// discriminator), e.g. `KORD 032151Z 23006KT ... RMK AO2 SLP162`.
fn is_ao2(raw_text: &str) -> bool {
    raw_text
        .split_once(" RMK ")
        .is_some_and(|(_, remarks)| remarks.split_whitespace().any(|token| token == "AO2"))
}

/// What one observation run wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationReport {
    pub written: usize,
    /// Reports dropped because a required field did not parse.
    pub skipped: usize,
}

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
        ])
        .build()
        .unwrap()
}

pub struct ObservationService {
    pub logger: Logger,
    pub fetcher: Arc<XmlFetcher>,
}
impl ObservationService {
    pub fn new(logger: Logger, fetcher: Arc<XmlFetcher>) -> Self {
        ObservationService { logger, fetcher }
    }

    /// Fetches observations and writes them directly to a parquet file.
    /// Returns the path to the written parquet file.
    pub async fn get_observations_to_file(
        &self,
        city_weather: &CityWeather,
        output_path: &str,
    ) -> Result<ObservationReport, Error> {
        let url = "https://aviationweather.gov/data/cache/metars.cache.xml.gz";
        info!(self.logger, "fetching observations from {}", url);
        let raw_observation = self.fetcher.fetch_xml_gzip(url).await?;
        let converted_xml: ObservationData = parse_xml(&raw_observation)?;

        // Create parquet writer
        let file = File::create(output_path)
            .map_err(|e| anyhow!("failed to create parquet file: {}", e))?;
        let props = WriterProperties::builder().build();
        let mut writer =
            SerializedFileWriter::new(file, Arc::new(create_observation_schema()), Arc::new(props))
                .map_err(|e| anyhow!("failed to create parquet writer: {}", e))?;

        let mut observations = vec![];
        let mut skipped = 0;
        for value in converted_xml.data.metar.iter() {
            if value.temp_c.is_none()
                || value.longitude.is_none()
                || value.latitude.is_none()
                || value.observation_time.is_none()
            {
                // skip reading if missing key values
                continue;
            }
            // One malformed report must not lose every station's hour.
            let converted = CurrentWeather::try_from(value.clone()).and_then(Observation::try_from);
            let mut observation = match converted {
                Ok(observation) => observation,
                Err(error) => {
                    skipped += 1;
                    warn!(
                        self.logger,
                        "skipping METAR for {}: {}", value.station_id, error
                    );
                    continue;
                }
            };
            if let Some(city) = city_weather.city_data.get(&observation.station_id) {
                // only add observation if we have a station_name with it
                observation.station_name = city.station_name.clone();
                observation.state = city.state.clone();
                observation.iata_id = city.iata_id.clone();
                observation.elevation_m = city.elevation_m;
                observations.push(observation)
            }
        }

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

        info!(self.logger, "done writing observations to {}", output_path);
        Ok(ObservationReport {
            written: observations.len(),
            skipped,
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
