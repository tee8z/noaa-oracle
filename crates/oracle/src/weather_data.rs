//! Weather queries over parquet files with an in-process DuckDB connection.
//!
//! Every query opens a fresh in-memory connection on a blocking thread, so
//! queries never stall the async runtime and share no state. A semaphore
//! bounds how many run at once, and each connection has memory and thread
//! limits. Station ids and file names are validated before they are
//! interpolated into SQL.
//!
//! Parquet files are append-only history written by every daemon version.
//! Each query unions a NULL-typed row declaring every column it reads, so
//! files that predate a column still load, and casts its output so decoded
//! Arrow types do not depend on which files matched. Decoding looks columns
//! up by name, never panics, and keeps missing values missing.

use crate::{
    file_access::{self, FileData, FileParams},
    routes::{ForecastRequest, ObservationRequest, TemperatureUnit},
};
use async_trait::async_trait;
use duckdb::{
    Connection,
    arrow::array::{Array, Float64Array, Int64Array, RecordBatch, StringArray},
};
use log::debug;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::{Duration, OffsetDateTime, Time, format_description::well_known::Rfc3339};
use tokio::sync::Semaphore;
use utoipa::ToSchema;

/// METAR present weather (`wx_string`) to `rain`, `snow`, or `ice`. Each
/// token is an optional intensity (`+`/`-`), an optional `VC`, then
/// two-letter groups: `-SN`, `+SHSN`, `-RASN`, `BLSN`, `-FZRA`, `PL`. Ice
/// wins over snow and snow over rain. Files without `wx_string` fall back
/// to temperature: at or below 2 °C counts as snow.
const PRECIP_TYPE_SQL: &str = r#"CASE
        WHEN wx_string IS NOT NULL AND wx_string != '' THEN
            CASE
                WHEN regexp_matches(wx_string, '(^|\s)[-+]?(VC)?(([A-Z]{2})*(PL|GR|GS|IC)|FZ(RA|DZ))(\s|$|[A-Z])') THEN 'ice'
                WHEN regexp_matches(wx_string, '(^|\s)[-+]?(VC)?([A-Z]{2})*(SN|SG)(\s|$|[A-Z])') THEN 'snow'
                ELSE 'rain'
            END
        WHEN temperature_value IS NOT NULL AND temperature_value <= 2.0 THEN 'snow'
        ELSE 'rain'
    END"#;

/// Observation history, back from the newest file, the station list is read from.
const STATION_LOOKBACK: Duration = Duration::days(30);

/// Queries running at once; more wait for a slot.
const MAX_CONCURRENT_QUERIES: usize = 4;
/// Per-connection DuckDB limits.
const QUERY_MEMORY_LIMIT: &str = "512MB";
const QUERY_THREADS: usize = 2;

pub struct WeatherAccess {
    file_access: Arc<dyn FileData>,
    slots: Arc<Semaphore>,
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
    #[error("Invalid station id: {0:?}")]
    InvalidStationId(String),
    #[error("query result column {column} is missing or not {expected}")]
    Schema {
        column: &'static str,
        expected: &'static str,
    },
    #[error("query task failed")]
    Task(#[from] tokio::task::JoinError),
}

const MAX_STATION_ID_LENGTH: usize = 16;

/// Station ids come from query strings and event definitions. Only ASCII
/// letters, digits, `-` and `_` are accepted so they can be quoted into SQL.
pub fn validate_station_id(station_id: &str) -> Result<(), Error> {
    let valid = !station_id.is_empty()
        && station_id.len() <= MAX_STATION_ID_LENGTH
        && station_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidStationId(station_id.to_string()))
    }
}

/// `WHERE station_id IN (...)` for the requested stations, or an empty
/// string when no filter applies.
fn station_filter(station_ids: &[String]) -> Result<String, Error> {
    if station_ids.is_empty() {
        return Ok(String::new());
    }
    for station_id in station_ids {
        validate_station_id(station_id)?;
    }
    Ok(format!(
        "WHERE station_id IN ({})",
        sql_string_list(station_ids)
    ))
}

/// Quotes values as a comma separated SQL string list.
fn sql_string_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("'{}'", value.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
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
    /// Get daily aggregated observations (grouped by UTC date)
    async fn daily_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<DailyObservation>, Error>;
    async fn stations(&self) -> Result<Vec<Station>, Error>;
}

/// The temperature unit a parquet row was written in. The daemon writes
/// `"celcius"` (sic); historical files keep that spelling, so both are
/// accepted.
fn source_unit(code: &str) -> Option<TemperatureUnit> {
    match code.to_ascii_lowercase().as_str() {
        "celsius" | "celcius" => Some(TemperatureUnit::Celsius),
        "fahrenheit" => Some(TemperatureUnit::Fahrenheit),
        _ => None,
    }
}

/// Converts `value` from `code` to `target`; unknown codes are left as is.
fn convert(value: f64, code: &str, target: &TemperatureUnit) -> f64 {
    match (source_unit(code), target) {
        (Some(TemperatureUnit::Celsius), TemperatureUnit::Fahrenheit) => value * 9.0 / 5.0 + 32.0,
        (Some(TemperatureUnit::Fahrenheit), TemperatureUnit::Celsius) => (value - 32.0) * 5.0 / 9.0,
        _ => value,
    }
}

/// The unit code values end up in after [`convert`].
fn converted_code(code: &str, target: &TemperatureUnit) -> String {
    match source_unit(code) {
        Some(_) => target.to_string(),
        None => code.to_owned(),
    }
}

impl WeatherAccess {
    pub fn new(file_access: Arc<dyn FileData>) -> Self {
        Self {
            file_access,
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_QUERIES)),
        }
    }

    /// Runs `sql` on a fresh connection off the async runtime and decodes
    /// the result there too.
    async fn query<T: Send + 'static>(
        &self,
        sql: String,
        decode: impl FnOnce(&[RecordBatch]) -> Result<Vec<T>, Error> + Send + 'static,
    ) -> Result<Vec<T>, Error> {
        let _slot = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Schema {
                column: "query slot",
                expected: "an open semaphore",
            })?;
        tokio::task::spawn_blocking(move || {
            let connection = open_connection()?;
            let mut statement = connection.prepare(&sql)?;
            let batches: Vec<RecordBatch> = statement.query_arrow([])?.collect();
            decode(&batches)
        })
        .await?
    }
}

/// A fresh in-memory connection with bounded memory and threads.
fn open_connection() -> Result<Connection, duckdb::Error> {
    let connection = Connection::open_in_memory()?;
    connection.execute_batch(&format!(
        "SET memory_limit = '{QUERY_MEMORY_LIMIT}'; SET threads = {QUERY_THREADS};
         INSTALL parquet; LOAD parquet;"
    ))?;
    Ok(connection)
}
#[async_trait]
impl WeatherData for WeatherAccess {
    async fn forecasts_data(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<Forecast>, Error> {
        let station_filter = station_filter(&station_ids)?;
        // If start is provided, look back one day to ensure we capture relevant files
        // If start is None, keep it None to find all available data
        let mut file_params: FileParams = req.into();
        if let Some(start_date) = req.start {
            file_params.start = Some(start_date.saturating_sub(Duration::days(1)));
        }
        let parquet_files = self.file_access.grab_file_names(file_params).await?;
        let file_paths = self.file_access.build_file_paths(parquet_files);
        if file_paths.is_empty() {
            return Ok(vec![]);
        }

        // Build time filter clauses for forecast period (begin_time/end_time)
        let mut time_filters = Vec::new();
        if let Some(start) = &req.start {
            time_filters.push(format!(
                "end_time::TIMESTAMPTZ > '{}'::TIMESTAMPTZ",
                start.format(&Rfc3339)?
            ));
        }
        if let Some(end) = &req.end {
            time_filters.push(format!(
                "begin_time::TIMESTAMPTZ < '{}'::TIMESTAMPTZ",
                end.format(&Rfc3339)?
            ));
        }

        let now = OffsetDateTime::now_utc();
        let (generated_start, generated_end) = match (req.generated_start, req.generated_end) {
            (Some(gs), Some(ge)) => (Some(gs), Some(ge)),
            (Some(gs), None) => (Some(gs), None),
            (None, Some(ge)) => (None, Some(ge)),
            (None, None) => {
                if let Some(start) = req.start {
                    let threshold = now + Duration::days(1);
                    if start <= threshold {
                        // Use start of the previous day to ensure we capture all relevant forecast files
                        // The DISTINCT ON ... ORDER BY generated_at DESC in SQL ensures we use the latest forecast
                        let prev_day_start = start
                            .date()
                            .previous_day()
                            .map(|d| d.with_time(Time::MIDNIGHT).assume_utc());
                        (prev_day_start, Some(now))
                    } else {
                        (Some(now.saturating_sub(Duration::days(1))), Some(now))
                    }
                } else {
                    (None, None)
                }
            }
        };

        if let Some(generated_start) = generated_start {
            time_filters.push(format!(
                "generated_at::TIMESTAMPTZ >= '{}'::TIMESTAMPTZ",
                generated_start.format(&Rfc3339)?
            ));
        }
        if let Some(generated_end) = generated_end {
            time_filters.push(format!(
                "generated_at::TIMESTAMPTZ <= '{}'::TIMESTAMPTZ",
                generated_end.format(&Rfc3339)?
            ));
        }

        let time_filter = if time_filters.is_empty() {
            String::new()
        } else if station_filter.is_empty() {
            format!("WHERE {}", time_filters.join(" AND "))
        } else {
            format!("AND {}", time_filters.join(" AND "))
        };

        // Build start/end time expressions for final select
        let start_time_expr = if let Some(start) = &req.start {
            format!(
                "GREATEST('{}', MIN(df.start_time))",
                start.format(&Rfc3339)?
            )
        } else {
            "MIN(df.start_time)".to_string()
        };
        let end_time_expr = if let Some(end) = &req.end {
            format!("LEAST('{}', MAX(df.end_time))", end.format(&Rfc3339)?)
        } else {
            "MAX(df.end_time)".to_string()
        };

        // Use raw SQL with UNION ALL BY NAME to handle schema differences
        // Old files may not have all columns - we define NULL defaults for backwards compatibility
        // For precipitation, we first deduplicate by taking the latest forecast for each unique time window,
        // then sum across time windows to get daily totals
        // Rain is calculated as: QPF - (snow_amt / snow_ratio), or just QPF if no snow_ratio
        let query_sql = format!(
            r#"
            WITH parquet_data AS (
                SELECT * FROM (
                    SELECT NULL::VARCHAR AS station_id, NULL::VARCHAR AS begin_time, NULL::VARCHAR AS end_time,
                           NULL::BIGINT AS min_temp, NULL::BIGINT AS max_temp, NULL::BIGINT AS wind_speed,
                           NULL::BIGINT AS wind_direction, NULL::BIGINT AS relative_humidity_max,
                           NULL::BIGINT AS relative_humidity_min,
                           NULL::VARCHAR AS temperature_unit_code, NULL::DOUBLE AS twelve_hour_probability_of_precipitation,
                           NULL::DOUBLE AS liquid_precipitation_amt, NULL::DOUBLE AS snow_amt,
                           NULL::DOUBLE AS snow_ratio, NULL::DOUBLE AS ice_amt,
                           NULL::VARCHAR AS generated_at
                    WHERE false
                    UNION ALL BY NAME
                    SELECT * FROM read_parquet([{}], union_by_name = true)
                )
            ),
            -- Deduplicate: for each station + time window (normalized to UTC), take the most recent forecast
            deduped_forecasts AS (
                SELECT DISTINCT ON (station_id, begin_time::TIMESTAMPTZ, end_time::TIMESTAMPTZ)
                    station_id,
                    begin_time,
                    end_time,
                    min_temp,
                    max_temp,
                    wind_speed,
                    wind_direction,
                    relative_humidity_max,
                    relative_humidity_min,
                    temperature_unit_code,
                    twelve_hour_probability_of_precipitation,
                    liquid_precipitation_amt,
                    snow_amt,
                    snow_ratio,
                    ice_amt,
                    generated_at
                FROM parquet_data
                {} {}
                ORDER BY station_id, begin_time::TIMESTAMPTZ, end_time::TIMESTAMPTZ, generated_at DESC
            ),
            -- Precipitation bucketing: rows exist at multiple interval durations (1h, 3h, 6h, 12h, 24h)
            -- Precipitation rows with duration info. Each precip field (QPF, snow, ice)
            -- may have a different native interval from NOAA, so we detect intervals per-field.
            precip_rows AS (
                SELECT
                    station_id,
                    DATE_TRUNC('day', begin_time::TIMESTAMPTZ AT TIME ZONE 'UTC')::TEXT AS date,
                    begin_time::TIMESTAMPTZ AS begin_ts,
                    end_time::TIMESTAMPTZ AS end_ts,
                    EXTRACT(EPOCH FROM (end_time::TIMESTAMPTZ - begin_time::TIMESTAMPTZ)) AS duration_secs,
                    liquid_precipitation_amt,
                    snow_amt,
                    snow_ratio,
                    ice_amt
                FROM deduped_forecasts
                WHERE liquid_precipitation_amt IS NOT NULL
                   OR snow_amt IS NOT NULL
                   OR ice_amt IS NOT NULL
            ),
            -- QPF: detect native interval for liquid precipitation
            qpf_duration AS (
                SELECT station_id, date, duration_secs, COUNT(*) AS row_count,
                    SUM(CASE WHEN next_begin IS NOT NULL AND end_ts = next_begin THEN 1 ELSE 0 END) AS chain_count
                FROM (
                    SELECT station_id, date, duration_secs, begin_ts, end_ts,
                        LEAD(begin_ts) OVER (PARTITION BY station_id, date, duration_secs ORDER BY begin_ts) AS next_begin
                    FROM precip_rows WHERE liquid_precipitation_amt IS NOT NULL
                ) sub
                GROUP BY station_id, date, duration_secs
                HAVING COUNT(*) > 1
            ),
            best_qpf_duration AS (
                SELECT DISTINCT ON (station_id, date) station_id, date, duration_secs
                FROM qpf_duration
                ORDER BY station_id, date, chain_count::FLOAT / row_count DESC, duration_secs ASC
            ),
            -- Snow: detect native interval for snow amount
            snow_duration AS (
                SELECT station_id, date, duration_secs, COUNT(*) AS row_count,
                    SUM(CASE WHEN next_begin IS NOT NULL AND end_ts = next_begin THEN 1 ELSE 0 END) AS chain_count
                FROM (
                    SELECT station_id, date, duration_secs, begin_ts, end_ts,
                        LEAD(begin_ts) OVER (PARTITION BY station_id, date, duration_secs ORDER BY begin_ts) AS next_begin
                    FROM precip_rows WHERE snow_amt IS NOT NULL
                ) sub
                GROUP BY station_id, date, duration_secs
                HAVING COUNT(*) > 1
            ),
            best_snow_duration AS (
                SELECT DISTINCT ON (station_id, date) station_id, date, duration_secs
                FROM snow_duration
                ORDER BY station_id, date, chain_count::FLOAT / row_count DESC, duration_secs ASC
            ),
            -- Ice: detect native interval for ice amount
            ice_duration AS (
                SELECT station_id, date, duration_secs, COUNT(*) AS row_count,
                    SUM(CASE WHEN next_begin IS NOT NULL AND end_ts = next_begin THEN 1 ELSE 0 END) AS chain_count
                FROM (
                    SELECT station_id, date, duration_secs, begin_ts, end_ts,
                        LEAD(begin_ts) OVER (PARTITION BY station_id, date, duration_secs ORDER BY begin_ts) AS next_begin
                    FROM precip_rows WHERE ice_amt IS NOT NULL
                ) sub
                GROUP BY station_id, date, duration_secs
                HAVING COUNT(*) > 1
            ),
            best_ice_duration AS (
                SELECT DISTINCT ON (station_id, date) station_id, date, duration_secs
                FROM ice_duration
                ORDER BY station_id, date, chain_count::FLOAT / row_count DESC, duration_secs ASC
            ),
            -- Sum each field using its own native duration.
            -- Fallback: when best_*_duration has no match (single-row days filtered by HAVING > 1),
            -- use the shortest available duration for that field.
            daily_qpf AS (
                SELECT pr.station_id, pr.date,
                    SUM(pr.liquid_precipitation_amt) FILTER (WHERE pr.liquid_precipitation_amt IS NOT NULL AND pr.liquid_precipitation_amt >= 0) AS total_qpf
                FROM precip_rows pr
                LEFT JOIN best_qpf_duration bqd ON pr.station_id = bqd.station_id AND pr.date = bqd.date
                WHERE pr.liquid_precipitation_amt IS NOT NULL
                  AND pr.duration_secs = COALESCE(bqd.duration_secs, (
                      SELECT MIN(p2.duration_secs) FROM precip_rows p2
                      WHERE p2.station_id = pr.station_id AND p2.date = pr.date AND p2.liquid_precipitation_amt IS NOT NULL
                  ))
                GROUP BY pr.station_id, pr.date
            ),
            daily_snow AS (
                SELECT pr.station_id, pr.date,
                    SUM(pr.snow_amt) FILTER (WHERE pr.snow_amt IS NOT NULL AND pr.snow_amt >= 0) AS snow_amt,
                    AVG(pr.snow_ratio) FILTER (WHERE pr.snow_ratio IS NOT NULL AND pr.snow_ratio > 0) AS avg_snow_ratio
                FROM precip_rows pr
                LEFT JOIN best_snow_duration bsd ON pr.station_id = bsd.station_id AND pr.date = bsd.date
                WHERE pr.snow_amt IS NOT NULL
                  AND pr.duration_secs = COALESCE(bsd.duration_secs, (
                      SELECT MIN(p2.duration_secs) FROM precip_rows p2
                      WHERE p2.station_id = pr.station_id AND p2.date = pr.date AND p2.snow_amt IS NOT NULL
                  ))
                GROUP BY pr.station_id, pr.date
            ),
            daily_ice AS (
                SELECT pr.station_id, pr.date,
                    SUM(pr.ice_amt) FILTER (WHERE pr.ice_amt IS NOT NULL AND pr.ice_amt >= 0) AS ice_amt
                FROM precip_rows pr
                LEFT JOIN best_ice_duration bid ON pr.station_id = bid.station_id AND pr.date = bid.date
                WHERE pr.ice_amt IS NOT NULL
                  AND pr.duration_secs = COALESCE(bid.duration_secs, (
                      SELECT MIN(p2.duration_secs) FROM precip_rows p2
                      WHERE p2.station_id = pr.station_id AND p2.date = pr.date AND p2.ice_amt IS NOT NULL
                  ))
                GROUP BY pr.station_id, pr.date
            ),
            -- Combine per-field daily sums
            daily_precip AS (
                SELECT
                    COALESCE(q.station_id, s.station_id, i.station_id) AS station_id,
                    COALESCE(q.date, s.date, i.date) AS date,
                    q.total_qpf,
                    s.snow_amt,
                    s.avg_snow_ratio,
                    i.ice_amt
                FROM daily_qpf q
                FULL OUTER JOIN daily_snow s ON q.station_id = s.station_id AND q.date = s.date
                FULL OUTER JOIN daily_ice i ON COALESCE(q.station_id, s.station_id) = i.station_id AND COALESCE(q.date, s.date) = i.date
            ),
            daily_forecasts AS (
                SELECT
                    station_id,
                    DATE_TRUNC('day', begin_time::TIMESTAMPTZ AT TIME ZONE 'UTC')::TEXT AS date,
                    MIN(begin_time) AS start_time,
                    MAX(end_time) AS end_time,
                    MIN(min_temp) FILTER (WHERE min_temp IS NOT NULL AND min_temp >= -200 AND min_temp <= 200) AS temp_low,
                    MAX(max_temp) FILTER (WHERE max_temp IS NOT NULL AND max_temp >= -200 AND max_temp <= 200) AS temp_high,
                    MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500) AS wind_speed,
                    -- For wind direction, use mode (most common) or just take max as approximation
                    MAX(wind_direction) FILTER (WHERE wind_direction IS NOT NULL AND wind_direction >= 0 AND wind_direction <= 360) AS wind_direction,
                    MAX(relative_humidity_max) FILTER (WHERE relative_humidity_max IS NOT NULL AND relative_humidity_max >= 0 AND relative_humidity_max <= 100) AS humidity_max,
                    MIN(relative_humidity_min) FILTER (WHERE relative_humidity_min IS NOT NULL AND relative_humidity_min >= 0 AND relative_humidity_min <= 100) AS humidity_min,
                    MAX(temperature_unit_code) AS temperature_unit_code,
                    MAX(twelve_hour_probability_of_precipitation) FILTER (WHERE twelve_hour_probability_of_precipitation IS NOT NULL) AS precip_chance
                FROM deduped_forecasts
                GROUP BY station_id, DATE_TRUNC('day', begin_time::TIMESTAMPTZ AT TIME ZONE 'UTC')::TEXT
            )
            SELECT
                df.station_id::VARCHAR AS station_id,
                df.date::VARCHAR AS date,
                ({})::VARCHAR AS start_time,
                ({})::VARCHAR AS end_time,
                MIN(df.temp_low)::BIGINT AS temp_low,
                MAX(df.temp_high)::BIGINT AS temp_high,
                MAX(df.wind_speed)::BIGINT AS wind_speed,
                MAX(df.wind_direction)::BIGINT AS wind_direction,
                MAX(df.humidity_max)::BIGINT AS humidity_max,
                MIN(df.humidity_min)::BIGINT AS humidity_min,
                MAX(df.temperature_unit_code)::VARCHAR AS temperature_unit_code,
                MAX(df.precip_chance)::DOUBLE AS precip_chance,
                -- Calculate rain: QPF - (snow / snow_ratio) - ice
                -- If no snow_ratio, treat all QPF as rain (minus ice)
                -- Never return negative values
                GREATEST(0, COALESCE(
                    dp.total_qpf - (dp.snow_amt / NULLIF(dp.avg_snow_ratio, 0)) - COALESCE(dp.ice_amt, 0),
                    dp.total_qpf - COALESCE(dp.ice_amt, 0)
                ))::DOUBLE AS rain_amt,
                dp.snow_amt::DOUBLE AS snow_amt,
                dp.ice_amt::DOUBLE AS ice_amt
            FROM daily_forecasts df
            LEFT JOIN daily_precip dp ON df.station_id = dp.station_id AND df.date = dp.date
            GROUP BY df.station_id, df.date, dp.total_qpf, dp.snow_amt, dp.avg_snow_ratio, dp.ice_amt
            "#,
            sql_string_list(&file_paths),
            station_filter,
            time_filter,
            start_time_expr,
            end_time_expr,
        );

        let unit = req.temperature_unit;
        self.query(query_sql, move |batches| decode_forecasts(batches, &unit))
            .await
    }

    async fn observation_data(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<Observation>, Error> {
        let station_filter = station_filter(&station_ids)?;
        // If start is provided, look back one day to ensure we capture relevant files
        // If start is None, keep it None to find all available data
        let mut file_params: FileParams = req.into();
        if let Some(start_date) = req.start {
            file_params.start = Some(start_date.saturating_sub(Duration::days(1)));
        }
        let parquet_files = self.file_access.grab_file_names(file_params).await?;
        let file_paths = self.file_access.build_file_paths(parquet_files);

        if file_paths.is_empty() {
            return Ok(vec![]);
        }

        // Build time filter clauses
        let mut time_filters = Vec::new();
        if let Some(start) = &req.start {
            time_filters.push(format!(
                "generated_at::TIMESTAMPTZ >= '{}'::TIMESTAMPTZ",
                start.format(&Rfc3339)?
            ));
        }
        if let Some(end) = &req.end {
            time_filters.push(format!(
                "generated_at::TIMESTAMPTZ <= '{}'::TIMESTAMPTZ",
                end.format(&Rfc3339)?
            ));
        }

        let time_filter = if time_filters.is_empty() {
            String::new()
        } else if station_filter.is_empty() {
            format!("WHERE {}", time_filters.join(" AND "))
        } else {
            format!("AND {}", time_filters.join(" AND "))
        };

        // Build start/end time expressions
        let start_time_expr = if let Some(start) = &req.start {
            format!("GREATEST('{}', MIN(generated_at))", start.format(&Rfc3339)?)
        } else {
            "MIN(generated_at)".to_string()
        };
        let end_time_expr = if let Some(end) = &req.end {
            format!("LEAST('{}', MAX(generated_at))", end.format(&Rfc3339)?)
        } else {
            "MAX(generated_at)".to_string()
        };

        // Use raw SQL with UNION ALL BY NAME to handle schema differences
        // Old parquet files may not have wind_direction, dewpoint_value, precip_in, or wx_string
        // Humidity is derived from temperature and dewpoint using the Magnus formula
        // Precipitation is split into rain/snow/ice by PRECIP_TYPE_SQL
        // precip_in is liquid equivalent; snow inches = precip_in * snow_ratio (default 10)
        let precip_type = PRECIP_TYPE_SQL;
        let query_sql = format!(
            r#"
            WITH parquet_data AS (
                SELECT * FROM (
                    SELECT NULL::VARCHAR AS station_id, NULL::VARCHAR AS generated_at,
                           NULL::DOUBLE AS temperature_value, NULL::BIGINT AS wind_speed,
                           NULL::BIGINT AS wind_direction,
                           NULL::DOUBLE AS dewpoint_value, NULL::DOUBLE AS precip_in,
                           NULL::VARCHAR AS temperature_unit_code,
                           NULL::VARCHAR AS wx_string
                    WHERE false
                    UNION ALL BY NAME
                    SELECT * FROM read_parquet([{}], union_by_name = true)
                )
                {} {}
            ),
            -- Classify each observation's precipitation type
            classified AS (
                SELECT *, {precip_type} AS precip_type
                FROM parquet_data
            )
            SELECT
                station_id::VARCHAR AS station_id,
                ({})::VARCHAR AS start_time,
                ({})::VARCHAR AS end_time,
                MIN(temperature_value)::DOUBLE AS temp_low,
                MAX(temperature_value)::DOUBLE AS temp_high,
                (MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500))::BIGINT AS wind_speed,
                MAX(temperature_unit_code)::VARCHAR AS temperature_unit_code,
                (MAX(wind_direction) FILTER (WHERE wind_direction IS NOT NULL AND wind_direction >= 0 AND wind_direction <= 360))::BIGINT AS wind_direction,
                -- Derive humidity from temperature and dewpoint using Magnus formula
                CASE
                    WHEN AVG(dewpoint_value) IS NOT NULL AND AVG(temperature_value) IS NOT NULL
                    THEN ROUND(100.0 * EXP((17.625 * AVG(dewpoint_value)) / (243.04 + AVG(dewpoint_value)))
                         / EXP((17.625 * AVG(temperature_value)) / (243.04 + AVG(temperature_value))))::BIGINT
                    ELSE NULL
                END::BIGINT AS humidity,
                -- Rain: sum precip_in where type is rain (already liquid inches)
                SUM(precip_in) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'rain')::DOUBLE AS rain_amt,
                -- Snow: precip_in * 10 (default snow ratio) to convert liquid equivalent to snow inches
                SUM(precip_in * 10.0) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'snow')::DOUBLE AS snow_amt,
                -- Ice: liquid equivalent inches (roughly 1:1)
                SUM(precip_in) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'ice')::DOUBLE AS ice_amt
            FROM classified
            GROUP BY station_id
            "#,
            sql_string_list(&file_paths),
            station_filter,
            time_filter,
            start_time_expr,
            end_time_expr,
        );

        let unit = req.temperature_unit;
        self.query(query_sql, move |batches| {
            decode_observations(batches, &unit)
        })
        .await
    }

    async fn daily_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<DailyObservation>, Error> {
        let station_filter = station_filter(&station_ids)?;
        let mut file_params: FileParams = req.into();
        if let Some(start_date) = req.start {
            file_params.start = Some(start_date.saturating_sub(Duration::days(1)));
        }
        let parquet_files = self.file_access.grab_file_names(file_params).await?;
        let file_paths = self.file_access.build_file_paths(parquet_files);

        if file_paths.is_empty() {
            return Ok(vec![]);
        }

        // Build time filter clauses
        let mut time_filters = Vec::new();
        if let Some(start) = &req.start {
            time_filters.push(format!(
                "generated_at::TIMESTAMPTZ >= '{}'::TIMESTAMPTZ",
                start.format(&Rfc3339)?
            ));
        }
        if let Some(end) = &req.end {
            time_filters.push(format!(
                "generated_at::TIMESTAMPTZ <= '{}'::TIMESTAMPTZ",
                end.format(&Rfc3339)?
            ));
        }

        let time_filter = if time_filters.is_empty() {
            String::new()
        } else if station_filter.is_empty() {
            format!("WHERE {}", time_filters.join(" AND "))
        } else {
            format!("AND {}", time_filters.join(" AND "))
        };

        // Use raw SQL with UNION ALL BY NAME to handle schema differences
        // Same precipitation classification as observation_data()
        let precip_type = PRECIP_TYPE_SQL;
        let query_sql = format!(
            r#"
            WITH parquet_data AS (
                SELECT * FROM (
                    SELECT NULL::VARCHAR AS station_id, NULL::VARCHAR AS generated_at,
                           NULL::DOUBLE AS temperature_value, NULL::BIGINT AS wind_speed,
                           NULL::BIGINT AS wind_direction,
                           NULL::DOUBLE AS dewpoint_value, NULL::DOUBLE AS precip_in,
                           NULL::VARCHAR AS temperature_unit_code,
                           NULL::VARCHAR AS wx_string
                    WHERE false
                    UNION ALL BY NAME
                    SELECT * FROM read_parquet([{}], union_by_name = true)
                )
                {} {}
            ),
            -- Classify each observation's precipitation type
            classified AS (
                SELECT *, {precip_type} AS precip_type
                FROM parquet_data
            )
            SELECT
                station_id::VARCHAR AS station_id,
                DATE_TRUNC('day', generated_at::TIMESTAMP)::VARCHAR AS date,
                (MIN(temperature_value) FILTER (WHERE temperature_value IS NOT NULL))::DOUBLE AS temp_low,
                (MAX(temperature_value) FILTER (WHERE temperature_value IS NOT NULL))::DOUBLE AS temp_high,
                (MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500))::BIGINT AS wind_speed,
                MAX(temperature_unit_code)::VARCHAR AS temperature_unit_code,
                (MAX(wind_direction) FILTER (WHERE wind_direction IS NOT NULL AND wind_direction >= 0 AND wind_direction <= 360))::BIGINT AS wind_direction,
                CASE
                    WHEN AVG(dewpoint_value) IS NOT NULL AND AVG(temperature_value) IS NOT NULL
                    THEN ROUND(100.0 * EXP((17.625 * AVG(dewpoint_value)) / (243.04 + AVG(dewpoint_value)))
                         / EXP((17.625 * AVG(temperature_value)) / (243.04 + AVG(temperature_value))))::BIGINT
                    ELSE NULL
                END::BIGINT AS humidity,
                SUM(precip_in) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'rain')::DOUBLE AS rain_amt,
                SUM(precip_in * 10.0) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'snow')::DOUBLE AS snow_amt,
                SUM(precip_in) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'ice')::DOUBLE AS ice_amt
            FROM classified
            GROUP BY station_id, DATE_TRUNC('day', generated_at::TIMESTAMP)::TEXT
            "#,
            sql_string_list(&file_paths),
            station_filter,
            time_filter,
        );

        let unit = req.temperature_unit;
        self.query(query_sql, move |batches| decode_daily(batches, &unit))
            .await
    }

    async fn stations(&self) -> Result<Vec<Station>, Error> {
        // Stations that reported in the newest month of data; scanning all
        // history would grow without bound as files accumulate.
        let mut parquet_files: Vec<(OffsetDateTime, String)> = self
            .file_access
            .grab_file_names(FileParams {
                start: None,
                end: None,
                observations: Some(true),
                forecasts: Some(false),
            })
            .await?
            .into_iter()
            .filter_map(|name| {
                file_access::ParquetFileName::parse(&name)
                    .ok()
                    .map(|file| (file.generated_at, name))
            })
            .collect();
        let newest = parquet_files
            .iter()
            .map(|(generated_at, _)| *generated_at)
            .max();
        if let Some(newest) = newest {
            parquet_files.retain(|(generated_at, _)| *generated_at >= newest - STATION_LOOKBACK);
        }
        let parquet_files: Vec<String> = parquet_files.into_iter().map(|(_, name)| name).collect();
        let file_paths = self.file_access.build_file_paths(parquet_files);
        if file_paths.is_empty() {
            return Ok(vec![]);
        }
        // Query station data with union_by_name to handle schema differences
        // between old files (without new columns) and new files (with state, iata_id, elevation_m)
        // We use a dummy row with NULL values to define columns that may not exist in old files,
        // then UNION ALL BY NAME merges everything and COALESCE handles NULLs
        let query_sql = format!(
            r#"
            SELECT DISTINCT
                station_id::VARCHAR AS station_id,
                COALESCE(station_name, '')::VARCHAR AS station_name,
                COALESCE(state, '')::VARCHAR AS state,
                COALESCE(iata_id, '')::VARCHAR AS iata_id,
                elevation_m::DOUBLE AS elevation_m,
                latitude::DOUBLE AS latitude,
                longitude::DOUBLE AS longitude
            FROM (
                SELECT NULL::VARCHAR AS station_id, NULL::VARCHAR AS station_name,
                       NULL::VARCHAR AS state, NULL::VARCHAR AS iata_id,
                       NULL::DOUBLE AS elevation_m, NULL::DOUBLE AS latitude, NULL::DOUBLE AS longitude
                WHERE false
                UNION ALL BY NAME
                SELECT * FROM read_parquet([{}], union_by_name = true)
            )
            "#,
            sql_string_list(&file_paths)
        );

        self.query(query_sql, decode_stations).await
    }
}

/// A result column by name, as the Arrow type the query cast it to.
fn column<'a, T: Array + 'static>(
    batch: &'a RecordBatch,
    name: &'static str,
    expected: &'static str,
) -> Result<&'a T, Error> {
    batch
        .schema()
        .index_of(name)
        .ok()
        .and_then(|index| batch.column(index).as_any().downcast_ref::<T>())
        .ok_or(Error::Schema {
            column: name,
            expected,
        })
}

fn strings<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a StringArray, Error> {
    column(batch, name, "VARCHAR")
}

fn integers<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a Int64Array, Error> {
    column(batch, name, "BIGINT")
}

fn doubles<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a Float64Array, Error> {
    column(batch, name, "DOUBLE")
}

fn text(array: &StringArray, row: usize) -> Option<String> {
    (!array.is_null(row)).then(|| array.value(row).to_owned())
}

fn integer(array: &Int64Array, row: usize) -> Option<i64> {
    (!array.is_null(row)).then(|| array.value(row))
}

fn double(array: &Float64Array, row: usize) -> Option<f64> {
    (!array.is_null(row)).then(|| array.value(row))
}

/// A station id from file contents, if it is one we would accept from a
/// client. Anything else is dropped so it never reaches HTML or SQL.
fn station(array: &StringArray, row: usize) -> Option<String> {
    text(array, row).filter(|id| validate_station_id(id).is_ok())
}

fn within(value: Option<i64>, range: std::ops::RangeInclusive<i64>) -> Option<i64> {
    value.filter(|value| range.contains(value))
}

fn non_negative(value: Option<f64>) -> Option<f64> {
    value.filter(|value| *value >= 0.0)
}

fn skipped(kind: &str, row: usize) {
    debug!("skipping {kind} row {row}: missing station or temperature");
}

fn decode_forecasts(
    batches: &[RecordBatch],
    unit: &TemperatureUnit,
) -> Result<Vec<Forecast>, Error> {
    let mut forecasts = vec![];
    for batch in batches {
        let station_id = strings(batch, "station_id")?;
        let date = strings(batch, "date")?;
        let start_time = strings(batch, "start_time")?;
        let end_time = strings(batch, "end_time")?;
        let temp_low = integers(batch, "temp_low")?;
        let temp_high = integers(batch, "temp_high")?;
        let wind_speed = integers(batch, "wind_speed")?;
        let wind_direction = integers(batch, "wind_direction")?;
        let humidity_max = integers(batch, "humidity_max")?;
        let humidity_min = integers(batch, "humidity_min")?;
        let unit_code = strings(batch, "temperature_unit_code")?;
        let precip_chance = doubles(batch, "precip_chance")?;
        let rain_amt = doubles(batch, "rain_amt")?;
        let snow_amt = doubles(batch, "snow_amt")?;
        let ice_amt = doubles(batch, "ice_amt")?;
        for row in 0..batch.num_rows() {
            let (Some(station_id), Some(date), Some(low), Some(high)) = (
                station(station_id, row),
                text(date, row),
                integer(temp_low, row),
                integer(temp_high, row),
            ) else {
                skipped("forecast", row);
                continue;
            };
            let code = text(unit_code, row).unwrap_or_default();
            let temperature = |value: i64| convert(value as f64, &code, unit).round() as i64;
            forecasts.push(Forecast {
                station_id,
                date,
                start_time: text(start_time, row).unwrap_or_default(),
                end_time: text(end_time, row).unwrap_or_default(),
                temp_low: temperature(low),
                temp_high: temperature(high),
                wind_speed: within(integer(wind_speed, row), 0..=500),
                wind_direction: within(integer(wind_direction, row), 0..=360),
                humidity_max: within(integer(humidity_max, row), 0..=100),
                humidity_min: within(integer(humidity_min, row), 0..=100),
                temp_unit_code: converted_code(&code, unit),
                precip_chance: double(precip_chance, row)
                    .filter(|chance| (0.0..=100.0).contains(chance))
                    .map(|chance| chance.round() as i64),
                rain_amt: non_negative(double(rain_amt, row)),
                snow_amt: non_negative(double(snow_amt, row)),
                ice_amt: non_negative(double(ice_amt, row)),
            });
        }
    }
    Ok(forecasts)
}

fn decode_observations(
    batches: &[RecordBatch],
    unit: &TemperatureUnit,
) -> Result<Vec<Observation>, Error> {
    let mut observations = vec![];
    for batch in batches {
        let station_id = strings(batch, "station_id")?;
        let start_time = strings(batch, "start_time")?;
        let end_time = strings(batch, "end_time")?;
        let temp_low = doubles(batch, "temp_low")?;
        let temp_high = doubles(batch, "temp_high")?;
        let wind_speed = integers(batch, "wind_speed")?;
        let unit_code = strings(batch, "temperature_unit_code")?;
        let wind_direction = integers(batch, "wind_direction")?;
        let humidity = integers(batch, "humidity")?;
        let rain_amt = doubles(batch, "rain_amt")?;
        let snow_amt = doubles(batch, "snow_amt")?;
        let ice_amt = doubles(batch, "ice_amt")?;
        for row in 0..batch.num_rows() {
            let (Some(station_id), Some(low), Some(high)) = (
                station(station_id, row),
                double(temp_low, row),
                double(temp_high, row),
            ) else {
                skipped("observation", row);
                continue;
            };
            let code = text(unit_code, row).unwrap_or_default();
            observations.push(Observation {
                station_id,
                start_time: text(start_time, row).unwrap_or_default(),
                end_time: text(end_time, row).unwrap_or_default(),
                temp_low: convert(low, &code, unit),
                temp_high: convert(high, &code, unit),
                wind_speed: within(integer(wind_speed, row), 0..=500),
                temp_unit_code: converted_code(&code, unit),
                wind_direction: within(integer(wind_direction, row), 0..=360),
                humidity: within(integer(humidity, row), 0..=100),
                rain_amt: non_negative(double(rain_amt, row)),
                snow_amt: non_negative(double(snow_amt, row)),
                ice_amt: non_negative(double(ice_amt, row)),
            });
        }
    }
    Ok(observations)
}

fn decode_daily(
    batches: &[RecordBatch],
    unit: &TemperatureUnit,
) -> Result<Vec<DailyObservation>, Error> {
    let mut observations = vec![];
    for batch in batches {
        let station_id = strings(batch, "station_id")?;
        let date = strings(batch, "date")?;
        let temp_low = doubles(batch, "temp_low")?;
        let temp_high = doubles(batch, "temp_high")?;
        let wind_speed = integers(batch, "wind_speed")?;
        let unit_code = strings(batch, "temperature_unit_code")?;
        let wind_direction = integers(batch, "wind_direction")?;
        let humidity = integers(batch, "humidity")?;
        let rain_amt = doubles(batch, "rain_amt")?;
        let snow_amt = doubles(batch, "snow_amt")?;
        let ice_amt = doubles(batch, "ice_amt")?;
        for row in 0..batch.num_rows() {
            let (Some(station_id), Some(date), Some(low), Some(high)) = (
                station(station_id, row),
                text(date, row),
                double(temp_low, row),
                double(temp_high, row),
            ) else {
                skipped("daily observation", row);
                continue;
            };
            let code = text(unit_code, row).unwrap_or_default();
            observations.push(DailyObservation {
                station_id,
                date,
                temp_low: convert(low, &code, unit),
                temp_high: convert(high, &code, unit),
                wind_speed: within(integer(wind_speed, row), 0..=500),
                temp_unit_code: converted_code(&code, unit),
                wind_direction: within(integer(wind_direction, row), 0..=360),
                humidity: within(integer(humidity, row), 0..=100),
                rain_amt: non_negative(double(rain_amt, row)),
                snow_amt: non_negative(double(snow_amt, row)),
                ice_amt: non_negative(double(ice_amt, row)),
            });
        }
    }
    Ok(observations)
}

fn decode_stations(batches: &[RecordBatch]) -> Result<Vec<Station>, Error> {
    let mut stations = vec![];
    for batch in batches {
        let station_id = strings(batch, "station_id")?;
        let station_name = strings(batch, "station_name")?;
        let state = strings(batch, "state")?;
        let iata_id = strings(batch, "iata_id")?;
        let elevation_m = doubles(batch, "elevation_m")?;
        let latitude = doubles(batch, "latitude")?;
        let longitude = doubles(batch, "longitude")?;
        for row in 0..batch.num_rows() {
            let (Some(station_id), Some(latitude), Some(longitude)) = (
                station(station_id, row),
                double(latitude, row),
                double(longitude, row),
            ) else {
                continue;
            };
            stations.push(Station {
                station_id,
                station_name: text(station_name, row).unwrap_or_default(),
                state: text(state, row).unwrap_or_default(),
                iata_id: text(iata_id, row).unwrap_or_default(),
                elevation_m: double(elevation_m, row),
                latitude,
                longitude,
            });
        }
    }
    Ok(stations)
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct Forecast {
    pub station_id: String,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub temp_low: i64,
    pub temp_high: i64,
    /// Knots
    pub wind_speed: Option<i64>,
    /// Wind direction in degrees (0-360, where 0/360 = North)
    pub wind_direction: Option<i64>,
    /// Maximum relative humidity (percent)
    pub humidity_max: Option<i64>,
    /// Minimum relative humidity (percent)
    pub humidity_min: Option<i64>,
    pub temp_unit_code: String,
    pub precip_chance: Option<i64>,
    /// Liquid precipitation (rain) amount in inches
    pub rain_amt: Option<f64>,
    /// Snow amount in inches
    pub snow_amt: Option<f64>,
    /// Ice accumulation in inches
    pub ice_amt: Option<f64>,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct Observation {
    pub station_id: String,
    pub start_time: String,
    pub end_time: String,
    pub temp_low: f64,
    pub temp_high: f64,
    /// Knots; missing when no report in the window had a speed
    pub wind_speed: Option<i64>,
    pub temp_unit_code: String,
    /// Wind direction in degrees (0-360, where 0/360 = North); missing for
    /// variable (`VRB`) or unreported winds
    pub wind_direction: Option<i64>,
    /// Relative humidity (percent)
    pub humidity: Option<i64>,
    /// Liquid precipitation (rain) amount in inches
    pub rain_amt: Option<f64>,
    /// Snow amount in inches
    pub snow_amt: Option<f64>,
    /// Ice accumulation in inches
    pub ice_amt: Option<f64>,
}

/// Daily aggregated observation (grouped by UTC date)
#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct DailyObservation {
    pub station_id: String,
    pub date: String,
    pub temp_low: f64,
    pub temp_high: f64,
    /// Knots
    pub wind_speed: Option<i64>,
    pub temp_unit_code: String,
    /// Wind direction in degrees (0-360, where 0/360 = North)
    pub wind_direction: Option<i64>,
    /// Relative humidity (percent)
    pub humidity: Option<i64>,
    /// Liquid precipitation (rain) amount in inches
    pub rain_amt: Option<f64>,
    /// Snow amount in inches
    pub snow_amt: Option<f64>,
    /// Ice accumulation in inches
    pub ice_amt: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Station {
    pub station_id: String,
    pub station_name: String,
    pub state: String,
    pub iata_id: String,
    pub elevation_m: Option<f64>,
    pub latitude: f64,
    pub longitude: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_ids_are_validated_before_reaching_sql() {
        for valid in ["KORD", "PFNO", "K1G3", "station_1", "abc-def"] {
            assert!(validate_station_id(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "KORD'",
            "KORD') OR 1=1 --",
            "K ORD",
            "K;ORD",
            "ABCDEFGHIJKLMNOPQ",
        ] {
            assert!(validate_station_id(invalid).is_err(), "{invalid}");
        }
        assert!(station_filter(&["KORD".into(), "bad'id".into()]).is_err());
        assert_eq!(
            station_filter(&["KORD".into(), "KSAW".into()]).unwrap(),
            "WHERE station_id IN ('KORD', 'KSAW')"
        );
        assert_eq!(station_filter(&[]).unwrap(), "");
    }

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../e2e/fixtures/weather_data/2026-01-17/observations_2026-01-17T17:16:19.76658783Z.parquet"
    );

    /// A data directory holding the historical fixture (no `precip_in` or
    /// `wx_string` columns) and `extra` files written by DuckDB `COPY`.
    fn data_dir(extra: &[(&str, &str)]) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let day = directory.path().join("2026-01-17");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::copy(
            FIXTURE,
            day.join("observations_2026-01-17T17:16:19.76658783Z.parquet"),
        )
        .unwrap();
        let connection = open_connection().unwrap();
        for (name, select) in extra {
            let path = day.join(name);
            connection
                .execute_batch(&format!(
                    "COPY ({select}) TO '{}' (FORMAT PARQUET)",
                    path.display()
                ))
                .unwrap();
        }
        directory
    }

    fn day_request() -> ObservationRequest {
        ObservationRequest {
            start: Some(time::macros::datetime!(2026-01-17 00:00 UTC)),
            end: Some(time::macros::datetime!(2026-01-18 00:00 UTC)),
            station_ids: "KORD,KSAW".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        }
    }

    fn access(directory: &tempfile::TempDir) -> WeatherAccess {
        WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
            directory.path().to_string_lossy().into_owned(),
        )))
    }

    #[tokio::test]
    async fn historical_and_current_observation_files_query_together() {
        // A newer file: adds precip_in and wx_string, plus a trailing column
        // no reader knows yet.
        let directory = data_dir(&[(
            "observations_2026-01-17T18:00:00Z.parquet",
            "SELECT 'KORD' AS station_id, 'Chicago' AS station_name, 41.9::DOUBLE AS latitude,
                    -87.9::DOUBLE AS longitude, '2026-01-17T17:51:00Z' AS generated_at,
                    -8.0::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code,
                    NULL::BIGINT AS wind_direction, 'degrees' AS wind_direction_unit_code,
                    NULL::BIGINT AS wind_speed, 'knots' AS wind_speed_unit_code,
                    -12.0::DOUBLE AS dewpoint_value, 'celcius' AS dewpoint_unit_code,
                    0.02::DOUBLE AS precip_in, '-SN' AS wx_string, TRUE AS wind_variable",
        )]);
        let request = day_request();
        let observations = access(&directory)
            .observation_data(&request, request.station_ids())
            .await
            .unwrap();
        let kord = observations
            .iter()
            .find(|observation| observation.station_id == "KORD")
            .unwrap();
        // -8 °C from the new file beats -10 °C from the old one.
        assert!((kord.temp_high - 17.6).abs() < 1e-9, "{}", kord.temp_high);
        assert_eq!(kord.temp_unit_code, "fahrenheit");
        assert!((kord.snow_amt.unwrap() - 0.2).abs() < 1e-9);
        let ksaw = observations
            .iter()
            .find(|observation| observation.station_id == "KSAW")
            .unwrap();
        assert_eq!(ksaw.rain_amt, None, "files without precip_in report none");
        assert_eq!(ksaw.wind_speed, Some(14));
    }

    #[tokio::test]
    async fn unusable_parquet_contents_are_errors_or_dropped_rows_not_panics() {
        let directory = data_dir(&[
            (
                "observations_2026-01-17T19:00:00Z.parquet",
                "SELECT 'KORD''); DROP' AS station_id, '2026-01-17T18:51:00Z' AS generated_at,
                        1.0::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code",
            ),
            (
                "observations_2026-01-17T20:00:00Z.parquet",
                "SELECT 'KSAW' AS station_id, '2026-01-17T19:51:00Z' AS generated_at,
                        NULL::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code",
            ),
        ]);
        let access = access(&directory);
        let stations = access.stations().await.unwrap();
        assert!(stations.iter().any(|station| station.station_id == "KORD"));
        assert!(
            stations
                .iter()
                .all(|station| validate_station_id(&station.station_id).is_ok()),
            "invalid ids from files never reach callers"
        );
        let request = ObservationRequest {
            station_ids: String::new(),
            ..day_request()
        };
        let observations = access.observation_data(&request, vec![]).await.unwrap();
        assert!(!observations.is_empty());
        assert!(
            observations
                .iter()
                .all(|observation| validate_station_id(&observation.station_id).is_ok())
        );

        let broken = data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT 'KSAW' AS station_id, '2026-01-17T20:51:00Z' AS generated_at,
                    'warm' AS temperature_value, 'celcius' AS temperature_unit_code",
        )]);
        let request = day_request();
        assert!(
            super::WeatherAccess::observation_data(
                &self::access(&broken),
                &request,
                request.station_ids()
            )
            .await
            .is_err(),
            "a column of the wrong type fails the query instead of panicking"
        );
    }

    #[test]
    fn temperature_units_accept_the_daemon_spelling() {
        assert!((convert(0.0, "celcius", &TemperatureUnit::Fahrenheit) - 32.0).abs() < 1e-9);
        assert!((convert(0.0, "Celsius", &TemperatureUnit::Fahrenheit) - 32.0).abs() < 1e-9);
        assert!((convert(212.0, "fahrenheit", &TemperatureUnit::Celsius) - 100.0).abs() < 1e-9);
        assert_eq!(convert(5.0, "kelvin", &TemperatureUnit::Celsius), 5.0);
        assert_eq!(
            converted_code("kelvin", &TemperatureUnit::Celsius),
            "kelvin"
        );
        assert_eq!(
            converted_code("celcius", &TemperatureUnit::Celsius),
            "celsius"
        );
    }

    #[test]
    fn metar_weather_codes_classify_precipitation() {
        let connection = open_connection().unwrap();
        let classify = |wx: &str, temperature: f64| -> String {
            connection
                .query_row(
                    &format!(
                        "SELECT {PRECIP_TYPE_SQL} FROM (SELECT ?::VARCHAR AS wx_string, ?::DOUBLE AS temperature_value)"
                    ),
                    duckdb::params![wx, temperature],
                    |row| row.get(0),
                )
                .unwrap()
        };
        for (wx, expected) in [
            ("SN", "snow"),
            ("-SN", "snow"),
            ("+SN", "snow"),
            ("-SHSN", "snow"),
            ("-RASN", "snow"),
            ("BLSN BR", "snow"),
            ("VCSHSN", "snow"),
            ("-SG", "snow"),
            ("-FZRA", "ice"),
            ("FZDZ", "ice"),
            ("-FZRA SN", "ice"),
            ("PL", "ice"),
            ("-SHGS", "ice"),
            ("RA", "rain"),
            ("-TSRA", "rain"),
            ("BR", "rain"),
            ("SQ", "rain"),
        ] {
            assert_eq!(classify(wx, 10.0), expected, "{wx}");
        }
        assert_eq!(
            classify("", 1.0),
            "snow",
            "no weather codes: temperature decides"
        );
        assert_eq!(classify("", 5.0), "rain");
    }

    #[test]
    fn sql_string_lists_escape_quotes() {
        assert_eq!(
            sql_string_list(&["a".into(), "it's".into()]),
            "'a', 'it''s'"
        );
    }
}
