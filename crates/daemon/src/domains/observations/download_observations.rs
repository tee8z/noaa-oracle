use anyhow::{anyhow, Error};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::record::RecordWriter;
use parquet::{
    basic::{LogicalType, Repetition, Type as PhysicalType},
    schema::types::Type,
};
use parquet_derive::ParquetRecordWriter;
use slog::{info, Logger};
use std::fs::File;
use std::sync::Arc;
use time::{format_description::well_known::Rfc3339, macros::format_description, OffsetDateTime};

use crate::{CityWeather, Metar, ObservationData, Units, XmlFetcher};

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
                &val.observation_time
                    .clone()
                    .unwrap_or(OffsetDateTime::now_utc().to_string()),
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
            precip_in: val
                .precip_in
                .unwrap_or(String::from(""))
                .parse::<f64>()
                .map(Some)
                .unwrap_or(None),
            precip_unit_code: Units::Inches.to_string(),
            wx_string: val.wx_string.unwrap_or_default(),
        })
    }
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

    let schema = Type::group_type_builder("observation")
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
        .unwrap();

    schema
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
    ) -> Result<String, Error> {
        let url = "https://aviationweather.gov/data/cache/metars.cache.xml.gz";
        info!(self.logger, "fetching observations from {}", url);
        let raw_observation = self.fetcher.fetch_xml_gzip(url).await?;
        let converted_xml: ObservationData = serde_xml_rs::from_str(&raw_observation)?;

        // Create parquet writer
        let file = File::create(output_path)
            .map_err(|e| anyhow!("failed to create parquet file: {}", e))?;
        let props = WriterProperties::builder().build();
        let mut writer =
            SerializedFileWriter::new(file, Arc::new(create_observation_schema()), Arc::new(props))
                .map_err(|e| anyhow!("failed to create parquet writer: {}", e))?;

        let mut observations = vec![];
        for value in converted_xml.data.metar.iter() {
            if value.temp_c.is_none()
                || value.longitude.is_none()
                || value.latitude.is_none()
                || value.observation_time.is_none()
            {
                // skip reading if missing key values
                continue;
            }
            let current: CurrentWeather = value.clone().try_into()?;

            let mut observation: Observation = current.try_into()?;
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
        Ok(output_path.to_string())
    }
}
