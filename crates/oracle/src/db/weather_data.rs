use crate::{
    file_access, FileAccess, FileData, FileParams, ForecastRequest, ObservationRequest,
    TemperatureUnit,
};
use async_trait::async_trait;
use duckdb::{
    arrow::array::{Float64Array, Int64Array, RecordBatch, StringArray},
    params_from_iter, Connection,
};
use regex::Regex;
use scooby::postgres::{select, with, Aliasable, Parameters, Select};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};
use utoipa::ToSchema;

pub struct WeatherAccess {
    file_access: Arc<dyn FileData>,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Failed to query duckdb: {0}")]
    Query(#[from] duckdb::Error),
    #[error("Failed to format time string: {0}")]
    TimeFormat(#[from] time::error::Format),
    #[error("Failed to parse time string: {0}")]
    TimeParse(#[from] time::error::Parse),
    #[error("Failed to access files: {0}")]
    FileAccess(#[from] file_access::Error),
}

#[async_trait]
pub trait WeatherData: Sync + Send {
    async fn forecasts_data(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<Forecast>, Error>;
    async fn observation_data(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<Observation>, Error>;
    async fn stations(&self) -> Result<Vec<Station>, Error>;
}

pub fn convert_temperature(value: f64, from_unit: &str, to_unit: &TemperatureUnit) -> f64 {
    match (from_unit.to_lowercase().as_str(), to_unit) {
        ("celsius", TemperatureUnit::Fahrenheit) => (value * 9.0 / 5.0) + 32.0,
        ("fahrenheit", TemperatureUnit::Celsius) => (value - 32.0) * 5.0 / 9.0,
        _ => value, // No conversion needed
    }
}

impl WeatherAccess {
    pub fn new(file_access: Arc<FileAccess>) -> Result<Self, duckdb::Error> {
        Ok(Self { file_access })
    }

    /// Creates new in-memory connection, making it so we always start with a fresh slate and no possible locking issues
    pub fn open_connection(&self) -> Result<Connection, duckdb::Error> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("INSTALL parquet; LOAD parquet;")?;
        Ok(conn)
    }

    pub async fn query(
        &self,
        select: Select,
        params: Vec<String>,
    ) -> Result<Vec<RecordBatch>, duckdb::Error> {
        let re = Regex::new(r"\$(\d+)").unwrap();
        let binding = select.to_string();
        let fixed_params = re.replace_all(&binding, "?");
        let conn = self.open_connection()?;
        let mut stmt = conn.prepare(&fixed_params)?;
        let sql_params = params_from_iter(params.iter());
        Ok(stmt.query_arrow(sql_params)?.collect())
    }
}
#[async_trait]
impl WeatherData for WeatherAccess {
    async fn forecasts_data(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<Forecast>, Error> {
        let start_back_one_day = if let Some(start_date) = req.start {
            start_date.saturating_sub(Duration::days(1))
        } else {
            OffsetDateTime::now_utc().saturating_sub(Duration::days(1))
        };
        let mut file_params: FileParams = req.into();
        file_params.start = Some(start_back_one_day);
        let parquet_files = self.file_access.grab_file_names(file_params).await?;
        let file_paths = self.file_access.build_file_paths(parquet_files);
        if file_paths.is_empty() {
            return Ok(vec![]);
        }
        let mut placeholders = Parameters::new();

        let mut daily_forecasts = select((
            "station_id",
            "DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT".as_("date"),
            "MIN(begin_time)".as_("start_time"),
            "MAX(end_time)".as_("end_time"),
            "MIN(min_temp) FILTER (WHERE min_temp IS NOT NULL AND min_temp >= -200 AND min_temp <= 200)".as_("temp_low"),
            "MAX(max_temp) FILTER (WHERE max_temp IS NOT NULL AND max_temp >= -200 AND max_temp <= 200)".as_("temp_high"),
            "MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500)".as_("wind_speed"),
            "MAX(temperature_unit_code)".as_("temperature_unit_code"), // assumes consistent units per grouping
        ))
        .from(format!(
            "read_parquet(['{}'], union_by_name = true)",
            file_paths.join("', '")
        ));

        let mut values: Vec<String> = vec![];
        if !station_ids.is_empty() {
            daily_forecasts = daily_forecasts.where_(format!(
                "station_id IN ({})",
                placeholders.next_n(station_ids.len())
            ));

            for station_id in station_ids {
                values.push(station_id);
            }
        }
        if let Some(start) = &req.start {
            daily_forecasts = daily_forecasts.where_(format!(
                "begin_time::TIMESTAMPTZ >= {}::TIMESTAMPTZ",
                placeholders.next()
            ));
            values.push(start.format(&Rfc3339)?.to_owned());
        }

        if let Some(end) = &req.end {
            daily_forecasts = daily_forecasts.where_(format!(
                "end_time::TIMESTAMPTZ <= {}::TIMESTAMPTZ",
                placeholders.next()
            ));
            values.push(end.format(&Rfc3339)?.to_owned());
        }
        daily_forecasts = daily_forecasts.group_by((
            "station_id",
            "DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT",
        ));

        let query = with("daily_forecasts")
            .as_(daily_forecasts)
            .select((
                "station_id",
                "date",
                if let Some(start) = &req.start {
                    format!("GREATEST('{}', MIN(start_time))", start.format(&Rfc3339)?)
                        .as_("start_time")
                } else {
                    "MIN(start_time)".as_("start_time")
                },
                if let Some(end) = &req.end {
                    format!("LEAST('{}', MAX(end_time))", end.format(&Rfc3339)?).as_("end_time")
                } else {
                    "MAX(end_time)".as_("end_time")
                },
                "MIN(temp_low)".as_("temp_low"),
                "MAX(temp_high)".as_("temp_high"),
                "MAX(wind_speed)".as_("wind_speed"),
                "MAX(temperature_unit_code)".as_("temperature_unit_code"),
            ))
            .from("daily_forecasts")
            .group_by(("station_id", "date"));

        let records = self.query(query, values).await?;

        let forecasts: Forecasts = records
            .iter()
            .map(|record| Forecasts::from_with_temp_unit(record, &req.temperature_unit))
            .fold(Forecasts::new(), |mut acc, forecast| {
                acc.merge(forecast);
                acc
            });

        Ok(forecasts.values)
    }

    async fn observation_data(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<Observation>, Error> {
        let start_back_one_day = if let Some(start_date) = req.start {
            start_date.saturating_sub(Duration::days(1))
        } else {
            OffsetDateTime::now_utc().saturating_sub(Duration::days(1))
        };
        let mut file_params: FileParams = req.into();
        file_params.start = Some(start_back_one_day);
        let parquet_files = self.file_access.grab_file_names(file_params).await?;
        let file_paths = self.file_access.build_file_paths(parquet_files);

        if file_paths.is_empty() {
            return Ok(vec![]);
        }

        if file_paths.is_empty() {
            return Ok(vec![]);
        }

        let mut placeholders = Parameters::new();
        let mut values: Vec<String> = vec![];

        let mut base_query = select((
            "station_id",
            "generated_at",
            "temperature_value",
            "wind_speed",
            "temperature_unit_code",
        ))
        .from(format!(
            "read_parquet(['{}'], union_by_name = true)",
            file_paths.join("', '")
        ));

        if !station_ids.is_empty() {
            base_query = base_query.where_(format!(
                "station_id IN ({})",
                placeholders.next_n(station_ids.len())
            ));

            for station_id in station_ids {
                values.push(station_id);
            }
        }

        let filtered_data = with("all_station_data").as_(base_query);
        let agg_query = filtered_data
            .select((
                "station_id",
                "MIN(generated_at)".as_("min_time"),
                "MAX(generated_at)".as_("max_time"),
                "MIN(temperature_value)".as_("temp_low"),
                "MAX(temperature_value)".as_("temp_high"),
                "MAX(wind_speed)".as_("wind_speed"),
                "MAX(temperature_unit_code)".as_("temperature_unit_code"),
            ))
            .from("all_station_data")
            .group_by("station_id");

        let agg_cte = with("station_aggregates").as_(agg_query);

        let mut final_query = agg_cte
            .select((
                "station_id",
                if let Some(start) = &req.start {
                    format!("GREATEST('{}', min_time)", start.format(&Rfc3339)?).as_("start_time")
                } else {
                    "min_time".as_("start_time")
                },
                if let Some(end) = &req.end {
                    format!("LEAST('{}', max_time)", end.format(&Rfc3339)?).as_("end_time")
                } else {
                    "max_time".as_("end_time")
                },
                "temp_low",
                "temp_high",
                "wind_speed",
                "temperature_unit_code",
            ))
            .from("station_aggregates");

        match (&req.start, &req.end) {
            (Some(start), Some(end)) => {
                final_query = final_query.where_(format!(
                    "GREATEST('{}', min_time) <= LEAST('{}', max_time)",
                    start.format(&Rfc3339)?,
                    end.format(&Rfc3339)?
                ));
            }
            (Some(start), None) => {
                final_query = final_query.where_(format!(
                    "GREATEST('{}', min_time) <= max_time",
                    start.format(&Rfc3339)?
                ));
            }
            (None, Some(end)) => {
                final_query = final_query.where_(format!(
                    "min_time <= LEAST('{}', max_time)",
                    end.format(&Rfc3339)?
                ));
            }
            (None, None) => {}
        }

        let records = self.query(final_query, values).await?;
        let observations: Observations = records
            .iter()
            .map(|record| Observations::from_with_temp_unit(record, &req.temperature_unit))
            .fold(Observations::new(), |mut acc, obs| {
                acc.merge(obs);
                acc
            });
        Ok(observations.values)
    }

    async fn stations(&self) -> Result<Vec<Station>, Error> {
        let now = OffsetDateTime::now_utc();
        let start = now.saturating_sub(Duration::hours(4_i64));
        let parquet_files = self
            .file_access
            .grab_file_names(FileParams {
                start: Some(start),
                end: Some(now),
                observations: Some(true),
                forecasts: Some(false),
            })
            .await?;
        let file_paths = self.file_access.build_file_paths(parquet_files);
        if file_paths.is_empty() {
            return Ok(vec![]);
        }
        let mut query =
            select(("station_id", "station_name", "latitude", "longitude")).from(format!(
                "read_parquet(['{}'], union_by_name = true)",
                file_paths.join("', '")
            ));
        query = query.group_by(("station_id", "station_name", "latitude", "longitude"));

        let records = self.query(query, vec![]).await?;

        let stations: Stations =
            records
                .iter()
                .map(|record| record.into())
                .fold(Stations::new(), |mut acc, obs| {
                    acc.merge(obs);
                    acc
                });

        Ok(stations.values)
    }
}

struct Forecasts {
    values: Vec<Forecast>,
}

impl Forecasts {
    pub fn new() -> Self {
        Forecasts { values: Vec::new() }
    }

    pub fn merge(&mut self, forecasts: Forecasts) -> &Forecasts {
        self.values.extend(forecasts.values);
        self
    }

    fn from_with_temp_unit(record_batch: &RecordBatch, target_unit: &TemperatureUnit) -> Self {
        let mut forecasts = Vec::new();
        let station_id_arr = record_batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 0");
        let date_arr = record_batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 1");
        let start_time_arr = record_batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 2");
        let end_time_arr = record_batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 3");
        let temp_low_arr = record_batch
            .column(4)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 4");
        let temp_high_arr = record_batch
            .column(5)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 5");
        let wind_speed_arr = record_batch
            .column(6)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 6");

        let temperature_unit_code_arr = record_batch
            .column(7)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 7");

        for row_index in 0..record_batch.num_rows() {
            let station_id = station_id_arr.value(row_index).to_owned();
            let date = date_arr.value(row_index).to_owned();
            let start_time = start_time_arr.value(row_index).to_owned();
            let end_time = end_time_arr.value(row_index).to_owned();
            let temp_low = temp_low_arr.value(row_index);
            let temp_high = temp_high_arr.value(row_index);

            let wind_speed_val = wind_speed_arr.value(row_index);
            //We set max expected wind speed to something outlandish for nulls
            let wind_speed = if wind_speed_val >= 0 && wind_speed_val <= 3000 {
                Some(wind_speed_val)
            } else {
                None
            };
            let temp_unit_code = temperature_unit_code_arr.value(row_index).to_owned();

            let mut forecast = Forecast {
                station_id,
                date,
                start_time,
                end_time,
                temp_low,
                temp_high,
                wind_speed,
                temp_unit_code,
            };
            forecast.convert_temperature(target_unit);
            forecasts.push(forecast);
        }

        Self { values: forecasts }
    }
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct Forecast {
    pub station_id: String,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub temp_low: i64,
    pub temp_high: i64,
    pub wind_speed: Option<i64>,
    pub temp_unit_code: String,
}

impl Forecast {
    pub fn convert_temperature(&mut self, target_unit: &TemperatureUnit) {
        // Normalize the current unit code to handle the "celcius" spelling in data
        // The spelling error comes from NOAA data directly
        let current_unit = match self.temp_unit_code.to_lowercase().as_str() {
            "celcius" => "celsius".to_string(),
            _ => self.temp_unit_code.to_lowercase(),
        };

        // Skip if already in the target unit
        if current_unit == target_unit.to_string() {
            return;
        }

        match (current_unit.as_str(), target_unit) {
            ("celsius", TemperatureUnit::Fahrenheit) => {
                self.temp_low = ((self.temp_low as f64) * 9.0 / 5.0 + 32.0).round() as i64;
                self.temp_high = ((self.temp_high as f64) * 9.0 / 5.0 + 32.0).round() as i64;
                self.temp_unit_code = target_unit.to_string();
            }
            ("fahrenheit", TemperatureUnit::Celsius) => {
                self.temp_low = ((self.temp_low as f64 - 32.0) * 5.0 / 9.0).round() as i64;
                self.temp_high = ((self.temp_high as f64 - 32.0) * 5.0 / 9.0).round() as i64;
                self.temp_unit_code = target_unit.to_string();
            }
            _ => (), // No conversion needed or unknown unit
        }
    }
}

struct Observations {
    values: Vec<Observation>,
}

impl Observations {
    pub fn new() -> Self {
        Observations { values: Vec::new() }
    }

    pub fn merge(&mut self, observations: Observations) -> &Observations {
        self.values.extend(observations.values);
        self
    }

    pub fn from_with_temp_unit(record_batch: &RecordBatch, target_unit: &TemperatureUnit) -> Self {
        let mut observations = Vec::new();
        let station_id_arr = record_batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 0");
        let start_time_arr = record_batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 1");
        let end_time_arr = record_batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 2");
        let temp_low_arr = record_batch
            .column(3)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 3");
        let temp_high_arr = record_batch
            .column(4)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 4");
        let wind_speed_arr = record_batch
            .column(5)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 5");

        let temperature_unit_code_arr = record_batch
            .column(6)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 6");

        for row_index in 0..record_batch.num_rows() {
            let station_id = station_id_arr.value(row_index).to_owned();
            let start_time = start_time_arr.value(row_index).to_owned();
            let end_time = end_time_arr.value(row_index).to_owned();
            let temp_low = temp_low_arr.value(row_index);
            let temp_high = temp_high_arr.value(row_index);
            let wind_speed = wind_speed_arr.value(row_index);
            let temp_unit_code = temperature_unit_code_arr.value(row_index).to_owned();

            let mut observation = Observation {
                station_id,
                start_time,
                end_time,
                temp_low,
                temp_high,
                wind_speed,
                temp_unit_code,
            };
            observation.convert_temperature(target_unit);
            observations.push(observation);
        }

        Self {
            values: observations,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct Observation {
    pub station_id: String,
    pub start_time: String,
    pub end_time: String,
    pub temp_low: f64,
    pub temp_high: f64,
    pub wind_speed: i64,
    pub temp_unit_code: String,
}

impl Observation {
    pub fn convert_temperature(&mut self, target_unit: &TemperatureUnit) {
        // Normalize the current unit code to handle the "celcius" spelling in data
        // The spelling error comes from NOAA data directly
        let current_unit = match self.temp_unit_code.to_lowercase().as_str() {
            "celcius" => "celsius".to_string(),
            _ => self.temp_unit_code.to_lowercase(),
        };

        // Skip if already in the target unit
        if current_unit == target_unit.to_string() {
            return;
        }

        match (current_unit.as_str(), target_unit) {
            ("celsius", TemperatureUnit::Fahrenheit) => {
                self.temp_low = (self.temp_low as f64) * 9.0 / 5.0 + 32.0;
                self.temp_high = (self.temp_high as f64) * 9.0 / 5.0 + 32.0;
                self.temp_unit_code = target_unit.to_string();
            }
            ("fahrenheit", TemperatureUnit::Celsius) => {
                self.temp_low = (self.temp_low as f64 - 32.0) * 5.0 / 9.0;
                self.temp_high = (self.temp_high as f64 - 32.0) * 5.0 / 9.0;
                self.temp_unit_code = target_unit.to_string();
            }
            _ => (), // No conversion needed or unknown unit
        }
    }
}

struct Stations {
    values: Vec<Station>,
}

impl Stations {
    pub fn new() -> Self {
        Stations { values: Vec::new() }
    }

    pub fn merge(&mut self, stations: Stations) -> &Stations {
        self.values.extend(stations.values);
        self
    }
}

impl From<&RecordBatch> for Stations {
    fn from(record_batch: &RecordBatch) -> Self {
        let mut stations = Vec::new();
        let station_id_arr = record_batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 0");
        let station_name_arr = record_batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 1");
        let latitude_arr = record_batch
            .column(2)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 2");
        let longitude_arr = record_batch
            .column(3)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 3");

        for row_index in 0..record_batch.num_rows() {
            let station_id = station_id_arr.value(row_index).to_owned();
            let station_name = station_name_arr.value(row_index).to_owned();
            let latitude = latitude_arr.value(row_index);
            let longitude = longitude_arr.value(row_index);

            stations.push(Station {
                station_id,
                station_name,
                latitude,
                longitude,
            });
        }

        Self { values: stations }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct Station {
    pub station_id: String,
    pub station_name: String,
    pub latitude: f64,
    pub longitude: f64,
}
