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
    file_access::{self, FileData, FileParams, ParquetFileName},
    routes::{ForecastRequest, ObservationRequest, TemperatureUnit},
};
use async_trait::async_trait;
use duckdb::{
    Connection,
    arrow::array::{Array, Float64Array, Int64Array, RecordBatch, StringArray},
};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};
use time::{Duration, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;

mod derived;
#[cfg(test)]
mod legacy;

pub use derived::DerivedForecasts;

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

// A station may appear in many downloaded snapshots with the same report
// instant. Prefer the newest publication, including corrected reports, before
// calculating extrema, averages, or precipitation totals.
const DEDUP_OBSERVATIONS_SQL: &str = r#"
    SELECT * EXCLUDE (filename)
    FROM parquet_data
    QUALIFY ROW_NUMBER() OVER (
        PARTITION BY station_id, generated_at::TIMESTAMPTZ
        ORDER BY regexp_extract(filename, '(^|/)observations_([^/]+)\.parquet$', 2)::TIMESTAMPTZ DESC,
                 filename DESC, temperature_value DESC, temperature_unit_code DESC,
                 wind_speed DESC, wind_direction DESC, dewpoint_value DESC, dewpoint_unit_code DESC,
                 precip_in DESC, wx_string DESC
    ) = 1
"#;

// Normalize each report before aggregating. Temperature extrema, the Magnus
// humidity formula, and the precipitation fallback all need one common unit.
const NORMALIZE_OBSERVATIONS_SQL: &str = r#"
    SELECT * EXCLUDE (temperature_value, dewpoint_value, temperature_unit_code, dewpoint_unit_code),
        CASE WHEN isfinite(temperature_value) THEN
            CASE lower(temperature_unit_code)
                WHEN 'fahrenheit' THEN (temperature_value - 32.0) * 5.0 / 9.0
                WHEN 'celsius' THEN temperature_value
                WHEN 'celcius' THEN temperature_value
            END
        END::DOUBLE AS temperature_value,
        CASE WHEN isfinite(dewpoint_value) THEN
            CASE lower(COALESCE(dewpoint_unit_code, temperature_unit_code))
                WHEN 'fahrenheit' THEN (dewpoint_value - 32.0) * 5.0 / 9.0
                WHEN 'celsius' THEN dewpoint_value
                WHEN 'celcius' THEN dewpoint_value
            END
        END::DOUBLE AS dewpoint_value,
        'celsius'::VARCHAR AS temperature_unit_code
    FROM deduped
"#;

/// A report or forecast issue can reach a snapshot after its validity window.
const PUBLICATION_GRACE: Duration = Duration::hours(24);
const FORECAST_LOOKBACK: Duration = Duration::days(7);

/// Observation history, back from the newest file, the station list is read from.
const STATION_LOOKBACK: Duration = Duration::days(30);

/// Forecast files published this recently get derived copies: the widest
/// window a page or the coordinator reads (seven days of issues before a
/// week of comparisons) with a day to spare.
const DERIVED_WINDOW: Duration = Duration::days(10);

/// Queries running at once; more wait for a slot.
const MAX_CONCURRENT_QUERIES: usize = 4;
/// Per-connection DuckDB limits.
const QUERY_MEMORY_LIMIT: &str = "512MB";
const QUERY_THREADS: usize = 2;

pub struct WeatherAccess {
    file_access: Arc<dyn FileData>,
    slots: Arc<Semaphore>,
    derived: Option<DerivedForecasts>,
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
    #[error("Failed to write derived forecasts: {0}")]
    Io(#[from] std::io::Error),
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

    /// [`WeatherData::forecasts_data`] with each `date` a calendar day at
    /// `days` from UTC, for a reader's local "today". Implementations
    /// without local days group by UTC day.
    async fn local_forecasts(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
        days: UtcOffset,
    ) -> Result<Vec<Forecast>, Error> {
        let _ = days;
        self.forecasts_data(req, station_ids).await
    }

    /// [`WeatherData::daily_observations`] with each `date` a calendar day
    /// at `days` from UTC. Implementations without local days group by UTC
    /// day.
    async fn local_daily_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
        days: UtcOffset,
    ) -> Result<Vec<DailyObservation>, Error> {
        let _ = days;
        self.daily_observations(req, station_ids).await
    }

    /// Makes query-ready copies of recent data files that lack one and
    /// drops old copies, stopping early when `stopping` is cancelled.
    /// Returns how many copies were made.
    async fn prepare_files(&self, stopping: &CancellationToken) -> Result<usize, Error> {
        let _ = stopping;
        Ok(0)
    }
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

/// Timestamp comparisons happen as instants; the API still returns RFC3339.
fn utc_timestamp_sql(expression: &str) -> String {
    format!("strftime(({expression}) AT TIME ZONE 'UTC', '%Y-%m-%dT%H:%M:%S.%fZ')")
}

/// The calendar day of the instant `expression`, as `YYYY-MM-DD 00:00:00`,
/// in UTC or at `days` from it.
fn day_sql(expression: &str, days: UtcOffset) -> String {
    if days == UtcOffset::UTC {
        format!("DATE_TRUNC('day', {expression} AT TIME ZONE 'UTC')::VARCHAR")
    } else {
        format!(
            "DATE_TRUNC('day', ({expression} AT TIME ZONE 'UTC') + INTERVAL '{} seconds')::VARCHAR",
            days.whole_seconds()
        )
    }
}

/// `station_id IN (...)` for the requested stations, or `None` when no
/// filter applies.
fn station_condition(station_ids: &[String]) -> Result<Option<String>, Error> {
    Ok(station_filter(station_ids)?
        .strip_prefix("WHERE ")
        .map(str::to_owned))
}

/// Declares every column forecast queries read, so files written before a
/// column existed still load.
const FORECAST_SOURCE_COLUMNS: &str = "
    SELECT NULL::VARCHAR AS station_id, NULL::VARCHAR AS begin_time, NULL::VARCHAR AS end_time,
           NULL::BIGINT AS min_temp, NULL::BIGINT AS max_temp, NULL::BIGINT AS wind_speed,
           NULL::BIGINT AS wind_direction, NULL::BIGINT AS relative_humidity_max,
           NULL::BIGINT AS relative_humidity_min,
           NULL::VARCHAR AS temperature_unit_code, NULL::DOUBLE AS twelve_hour_probability_of_precipitation,
           NULL::DOUBLE AS liquid_precipitation_amt, NULL::DOUBLE AS snow_amt,
           NULL::DOUBLE AS snow_ratio, NULL::DOUBLE AS ice_amt,
           NULL::VARCHAR AS generated_at, NULL::VARCHAR AS filename
    WHERE false";

/// The columns of a forecast row as queries read it, in order. Derived
/// forecast files store exactly these.
const FORECAST_ROW_COLUMNS: &str = "station_id, begin_ts, end_ts, generated_ts, published_ts, source, \
     min_temp, max_temp, wind_speed, wind_direction, relative_humidity_max, relative_humidity_min, \
     precip_chance, liquid_precipitation_amt, snow_amt, snow_ratio, ice_amt";

/// Forecast rows from daemon files: times as instants, temperatures in
/// Fahrenheit, and the publication each row came from (`published_ts` from
/// the file name, `source` as `<date>/<file name>`), which orders repeated
/// issues. `filter` applies to the files' own columns, before any
/// conversion.
fn source_forecast_rows_sql(files: &[String], filter: &str) -> String {
    format!(
        r#"
        SELECT station_id,
            begin_time::TIMESTAMPTZ AS begin_ts,
            end_time::TIMESTAMPTZ AS end_ts,
            generated_at::TIMESTAMPTZ AS generated_ts,
            regexp_extract(filename, '(^|/)forecasts_([^/]+)\.parquet$', 2)::TIMESTAMPTZ AS published_ts,
            regexp_extract(filename, '[^/]+/[^/]+$') AS source,
            CASE lower(temperature_unit_code)
                WHEN 'fahrenheit' THEN min_temp
                WHEN 'celsius' THEN min_temp * 9.0 / 5.0 + 32.0
                WHEN 'celcius' THEN min_temp * 9.0 / 5.0 + 32.0
            END::DOUBLE AS min_temp,
            CASE lower(temperature_unit_code)
                WHEN 'fahrenheit' THEN max_temp
                WHEN 'celsius' THEN max_temp * 9.0 / 5.0 + 32.0
                WHEN 'celcius' THEN max_temp * 9.0 / 5.0 + 32.0
            END::DOUBLE AS max_temp,
            wind_speed,
            wind_direction,
            relative_humidity_max,
            relative_humidity_min,
            twelve_hour_probability_of_precipitation AS precip_chance,
            liquid_precipitation_amt,
            snow_amt,
            snow_ratio,
            ice_amt
        FROM ({FORECAST_SOURCE_COLUMNS}
              UNION ALL BY NAME
              SELECT * FROM read_parquet([{}], union_by_name = true, filename = true))
        {filter}"#,
        sql_string_list(files)
    )
}

/// Where a forecast query reads its rows: derived copies where they exist,
/// the published files otherwise.
#[derive(Debug, Default)]
struct ForecastFiles {
    derived: Vec<String>,
    source: Vec<String>,
}

impl ForecastFiles {
    fn is_empty(&self) -> bool {
        self.derived.is_empty() && self.source.is_empty()
    }
}

/// One precipitation field summed per station and day. NOAA publishes each
/// field at several interval lengths (1, 3, 6, 12 hours...). Per day, the
/// length whose periods chain end-to-start most completely is summed; ties
/// go to the shorter length. A day where no length has two periods sums
/// its shortest length.
fn precipitation_sql(name: &str, value: &str, extra_columns: &str, sums: &str) -> String {
    format!(
        r#"
    {name}_lengths AS (
        SELECT station_id, date, duration_secs, COUNT(*) AS row_count,
            SUM(CASE WHEN next_begin IS NOT NULL AND end_ts = next_begin THEN 1 ELSE 0 END) AS chain_count,
            {sums}
        FROM (
            SELECT station_id, date, begin_ts, end_ts, duration_secs, {value}{extra_columns},
                LEAD(begin_ts) OVER (PARTITION BY station_id, date, duration_secs ORDER BY begin_ts) AS next_begin
            FROM deduped
            WHERE {value} IS NOT NULL
        )
        GROUP BY station_id, date, duration_secs
    ),
    {name}_daily AS (
        SELECT * EXCLUDE (duration_secs, row_count, chain_count)
        FROM {name}_lengths
        QUALIFY ROW_NUMBER() OVER (
            PARTITION BY station_id, date
            ORDER BY row_count > 1 DESC,
                     CASE WHEN row_count > 1 THEN chain_count::FLOAT / row_count END DESC,
                     duration_secs ASC
        ) = 1
    ),"#
    )
}

/// Daily forecasts from `files`: for each station and forecast period, the
/// newest issue generated in the requested window, then per calendar day
/// (UTC, or at `days` from it) the extremes, peak wind with its direction,
/// humidity range, precipitation chance, and precipitation totals. Rain is
/// QPF minus the liquid equivalent of snow and ice, never below zero.
fn forecasts_sql(
    files: &ForecastFiles,
    stations: Option<&str>,
    req: &ForecastRequest,
    (generated_start, generated_end): (OffsetDateTime, OffsetDateTime),
    days: UtcOffset,
) -> Result<String, Error> {
    let mut conditions = Vec::new();
    if let Some(start) = &req.start {
        conditions.push(format!(
            "end_ts > '{}'::TIMESTAMPTZ",
            start.format(&Rfc3339)?
        ));
    }
    if let Some(end) = &req.end {
        conditions.push(format!(
            "begin_ts < '{}'::TIMESTAMPTZ",
            end.format(&Rfc3339)?
        ));
    }
    conditions.push(format!(
        "generated_ts >= '{}'::TIMESTAMPTZ",
        generated_start.format(&Rfc3339)?
    ));
    conditions.push(format!(
        "generated_ts <= '{}'::TIMESTAMPTZ",
        generated_end.format(&Rfc3339)?
    ));
    let times = conditions.join(" AND ");

    let mut branches = Vec::new();
    if !files.derived.is_empty() {
        let stations = stations
            .map(|condition| format!("{condition} AND "))
            .unwrap_or_default();
        branches.push(format!(
            "SELECT {FORECAST_ROW_COLUMNS} FROM read_parquet([{}]) WHERE {stations}{times}",
            sql_string_list(&files.derived)
        ));
    }
    if !files.source.is_empty() {
        let stations = stations
            .map(|condition| format!("WHERE {condition}"))
            .unwrap_or_default();
        branches.push(format!(
            "SELECT {FORECAST_ROW_COLUMNS} FROM ({}) WHERE {times}",
            source_forecast_rows_sql(&files.source, &stations)
        ));
    }
    let rows = branches.join("\n        UNION ALL\n        ");

    let start_time = match &req.start {
        Some(start) => format!(
            "GREATEST('{}'::TIMESTAMPTZ, d.start_time)",
            start.format(&Rfc3339)?
        ),
        None => "d.start_time".to_owned(),
    };
    let end_time = match &req.end {
        Some(end) => format!(
            "LEAST('{}'::TIMESTAMPTZ, d.end_time)",
            end.format(&Rfc3339)?
        ),
        None => "d.end_time".to_owned(),
    };
    let date = day_sql("begin_ts", days);
    let qpf = precipitation_sql(
        "qpf",
        "liquid_precipitation_amt",
        "",
        "SUM(liquid_precipitation_amt) FILTER (WHERE liquid_precipitation_amt >= 0) AS total_qpf",
    );
    let snow = precipitation_sql(
        "snow",
        "snow_amt",
        ", snow_ratio",
        "SUM(snow_amt) FILTER (WHERE snow_amt >= 0) AS snow_amt,
            SUM(CASE WHEN snow_amt = 0 THEN 0 ELSE snow_amt / snow_ratio END)
                FILTER (WHERE snow_amt >= 0 AND snow_ratio > 0) AS snow_liquid_amt",
    );
    let ice = precipitation_sql(
        "ice",
        "ice_amt",
        "",
        "SUM(ice_amt) FILTER (WHERE ice_amt >= 0) AS ice_amt",
    );
    Ok(format!(
        r#"
    WITH forecast_rows AS (
        {rows}
    ),
    -- Per station and period, the newest issue, then the newest
    -- publication of that issue. Value ties are deterministic.
    deduped AS (
        SELECT station_id, begin_ts, end_ts, min_temp, max_temp, wind_speed, wind_direction,
            relative_humidity_max, relative_humidity_min, precip_chance,
            liquid_precipitation_amt, snow_amt, snow_ratio, ice_amt,
            {date} AS date,
            EXTRACT(EPOCH FROM (end_ts - begin_ts)) AS duration_secs
        FROM forecast_rows
        QUALIFY ROW_NUMBER() OVER (
            PARTITION BY station_id, begin_ts, end_ts
            ORDER BY generated_ts DESC, published_ts DESC, source DESC,
                     max_temp DESC, min_temp DESC, wind_speed DESC, wind_direction DESC,
                     relative_humidity_max DESC, relative_humidity_min DESC,
                     precip_chance DESC, liquid_precipitation_amt DESC,
                     snow_amt DESC, snow_ratio DESC, ice_amt DESC
        ) = 1
    ),{qpf}{snow}{ice}
    daily AS (
        SELECT
            station_id,
            date,
            MIN(begin_ts) AS start_time,
            MAX(end_ts) AS end_time,
            MIN(min_temp) FILTER (WHERE min_temp >= -200 AND min_temp <= 200) AS temp_low,
            MAX(max_temp) FILTER (WHERE max_temp >= -200 AND max_temp <= 200) AS temp_high,
            MAX(wind_speed) FILTER (WHERE wind_speed >= 0 AND wind_speed <= 500) AS wind_speed,
            -- Direction belongs to the strongest wind period; a missing
            -- direction there must not fall back to a weaker period.
            FIRST(wind_direction ORDER BY wind_speed DESC, begin_ts DESC, end_ts DESC)
                FILTER (WHERE wind_speed >= 0 AND wind_speed <= 500) AS wind_direction,
            MAX(relative_humidity_max) FILTER (WHERE relative_humidity_max >= 0 AND relative_humidity_max <= 100) AS humidity_max,
            MIN(relative_humidity_min) FILTER (WHERE relative_humidity_min >= 0 AND relative_humidity_min <= 100) AS humidity_min,
            MAX(precip_chance) AS precip_chance
        FROM deduped
        GROUP BY station_id, date
    )
    SELECT
        d.station_id::VARCHAR AS station_id,
        d.date::VARCHAR AS date,
        ({})::VARCHAR AS start_time,
        ({})::VARCHAR AS end_time,
        d.temp_low::BIGINT AS temp_low,
        d.temp_high::BIGINT AS temp_high,
        d.wind_speed::BIGINT AS wind_speed,
        d.wind_direction::BIGINT AS wind_direction,
        d.humidity_max::BIGINT AS humidity_max,
        d.humidity_min::BIGINT AS humidity_min,
        'fahrenheit'::VARCHAR AS temperature_unit_code,
        d.precip_chance::DOUBLE AS precip_chance,
        CASE WHEN q.total_qpf IS NULL THEN NULL ELSE GREATEST(0,
            q.total_qpf - COALESCE(s.snow_liquid_amt, 0) - COALESCE(i.ice_amt, 0)
        ) END::DOUBLE AS rain_amt,
        s.snow_amt::DOUBLE AS snow_amt,
        i.ice_amt::DOUBLE AS ice_amt
    FROM daily d
    LEFT JOIN qpf_daily q ON d.station_id = q.station_id AND d.date = q.date
    LEFT JOIN snow_daily s ON d.station_id = s.station_id AND d.date = s.date
    LEFT JOIN ice_daily i ON d.station_id = i.station_id AND d.date = i.date
    ORDER BY d.station_id, d.date
    "#,
        utc_timestamp_sql(&start_time),
        utc_timestamp_sql(&end_time),
    ))
}

fn observation_file_params(req: &ObservationRequest, now: OffsetDateTime) -> FileParams {
    FileParams {
        start: req.start.map(|start| {
            start
                .saturating_sub(Duration::days(1))
                .to_offset(UtcOffset::UTC)
        }),
        end: Some(
            req.end
                .map(|end| end.saturating_add(PUBLICATION_GRACE).min(now))
                .unwrap_or(now)
                .to_offset(UtcOffset::UTC),
        ),
        observations: Some(true),
        forecasts: Some(false),
    }
}

fn forecast_generated_window(
    req: &ForecastRequest,
    now: OffsetDateTime,
) -> (OffsetDateTime, OffsetDateTime) {
    let end = req
        .generated_end
        .unwrap_or_else(|| req.end.unwrap_or(now).min(now));
    let start = req
        .generated_start
        .unwrap_or_else(|| end.saturating_sub(FORECAST_LOOKBACK));
    (start, end)
}

fn empty_publication_window(params: &FileParams) -> bool {
    matches!((params.start, params.end), (Some(start), Some(end)) if start > end)
}

impl WeatherAccess {
    /// Reads published files only.
    pub fn new(file_access: Arc<dyn FileData>) -> Self {
        Self {
            file_access,
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_QUERIES)),
            derived: None,
        }
    }

    /// Also keeps query-ready copies of recent forecast files in
    /// `directory` (see [`DerivedForecasts`]) and reads them instead.
    pub fn with_derived_forecasts(file_access: Arc<dyn FileData>, directory: &Path) -> Self {
        Self {
            derived: Some(DerivedForecasts::new(directory)),
            ..Self::new(file_access)
        }
    }

    /// The derived copy of each forecast file where one exists, and the
    /// published file where not.
    fn forecast_files(&self, file_names: Vec<String>) -> ForecastFiles {
        let mut files = ForecastFiles::default();
        for name in file_names {
            let Ok(file) = ParquetFileName::parse(&name) else {
                continue;
            };
            match self
                .derived
                .as_ref()
                .and_then(|derived| derived.existing(&file))
            {
                Some(copy) => files.derived.push(copy),
                None => files.source.push(self.file_access.build_file_path(&file)),
            }
        }
        files
    }

    async fn slot(&self) -> Result<tokio::sync::OwnedSemaphorePermit, Error> {
        self.slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Schema {
                column: "query slot",
                expected: "an open semaphore",
            })
    }

    /// Runs `sql` on a fresh connection off the async runtime and decodes
    /// the result there too.
    async fn query<T: Send + 'static>(
        &self,
        sql: String,
        decode: impl FnOnce(&[RecordBatch]) -> Result<Vec<T>, Error> + Send + 'static,
    ) -> Result<Vec<T>, Error> {
        let _slot = self.slot().await?;
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
        self.local_forecasts(req, station_ids, UtcOffset::UTC).await
    }

    async fn local_forecasts(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
        days: UtcOffset,
    ) -> Result<Vec<Forecast>, Error> {
        let stations = station_condition(&station_ids)?;
        let now = OffsetDateTime::now_utc();
        let generated = forecast_generated_window(req, now);
        // Files are named by publication time, not forecast validity. A
        // qualifying issue can be published after the generation cutoff.
        let file_params = FileParams {
            start: Some(generated.0.to_offset(UtcOffset::UTC)),
            end: Some(
                generated
                    .1
                    .saturating_add(PUBLICATION_GRACE)
                    .min(now)
                    .to_offset(UtcOffset::UTC),
            ),
            observations: Some(false),
            forecasts: Some(true),
        };
        if empty_publication_window(&file_params) {
            return Ok(vec![]);
        }
        let parquet_files = self.file_access.grab_file_names(file_params).await?;
        let files = self.forecast_files(parquet_files);
        if files.is_empty() {
            return Ok(vec![]);
        }
        let query_sql = forecasts_sql(&files, stations.as_deref(), req, generated, days)?;
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
        let file_params = observation_file_params(req, OffsetDateTime::now_utc());
        if empty_publication_window(&file_params) {
            return Ok(vec![]);
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
            format!(
                "GREATEST('{}'::TIMESTAMPTZ, MIN(generated_at::TIMESTAMPTZ))",
                start.format(&Rfc3339)?
            )
        } else {
            "MIN(generated_at::TIMESTAMPTZ)".to_string()
        };
        let end_time_expr = if let Some(end) = &req.end {
            format!(
                "LEAST('{}'::TIMESTAMPTZ, MAX(generated_at::TIMESTAMPTZ))",
                end.format(&Rfc3339)?
            )
        } else {
            "MAX(generated_at::TIMESTAMPTZ)".to_string()
        };

        // Use raw SQL with UNION ALL BY NAME to handle schema differences
        // Old parquet files may not have wind_direction, dewpoint_value, precip_in, or wx_string
        // Humidity is derived from temperature and dewpoint using the Magnus formula
        // Precipitation is split into rain/snow/ice by PRECIP_TYPE_SQL
        // precip_in is liquid equivalent; snow inches = precip_in * snow_ratio (default 10)
        let precip_type = PRECIP_TYPE_SQL;
        let dedup = DEDUP_OBSERVATIONS_SQL;
        let normalized = NORMALIZE_OBSERVATIONS_SQL;
        let query_sql = format!(
            r#"
            WITH parquet_data AS (
                SELECT * FROM (
                    SELECT NULL::VARCHAR AS station_id, NULL::VARCHAR AS generated_at,
                           NULL::DOUBLE AS temperature_value, NULL::BIGINT AS wind_speed,
                           NULL::BIGINT AS wind_direction,
                           NULL::DOUBLE AS dewpoint_value, NULL::DOUBLE AS precip_in,
                           NULL::VARCHAR AS temperature_unit_code,
                           NULL::VARCHAR AS wx_string, NULL::VARCHAR AS filename,
                           NULL::VARCHAR AS dewpoint_unit_code
                    WHERE false
                    UNION ALL BY NAME
                    SELECT * FROM read_parquet([{}], union_by_name = true, filename = true)
                )
                {} {}
            ),
            deduped AS ({dedup}),
            normalized AS ({normalized}),
            -- Classify each observation's precipitation type
            classified AS (
                SELECT *, {precip_type} AS precip_type
                FROM normalized
            )
            SELECT
                station_id::VARCHAR AS station_id,
                ({})::VARCHAR AS start_time,
                ({})::VARCHAR AS end_time,
                MIN(temperature_value)::DOUBLE AS temp_low,
                MAX(temperature_value)::DOUBLE AS temp_high,
                -- Keep the latest usable temperature, its timestamp, and its
                -- source unit from the same report. A newer report without a
                -- temperature must not make an older reading look fresh.
                FIRST(temperature_value ORDER BY generated_at::TIMESTAMPTZ DESC,
                      temperature_value DESC, temperature_unit_code DESC)
                    FILTER (WHERE temperature_value IS NOT NULL AND isfinite(temperature_value))::DOUBLE AS latest_temp,
                FIRST(generated_at ORDER BY generated_at::TIMESTAMPTZ DESC,
                      temperature_value DESC, temperature_unit_code DESC)
                    FILTER (WHERE temperature_value IS NOT NULL AND isfinite(temperature_value))::VARCHAR AS latest_temp_time,
                FIRST(temperature_unit_code ORDER BY generated_at::TIMESTAMPTZ DESC,
                      temperature_value DESC, temperature_unit_code DESC)
                    FILTER (WHERE temperature_value IS NOT NULL AND isfinite(temperature_value))::VARCHAR AS latest_temp_unit_code,
                (MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500))::BIGINT AS wind_speed,
                MAX(temperature_unit_code)::VARCHAR AS temperature_unit_code,
                FIRST(wind_direction ORDER BY wind_speed DESC, generated_at::TIMESTAMPTZ DESC)
                    FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500)::BIGINT AS wind_direction,
                -- Derive humidity from temperature and dewpoint using Magnus formula
                CASE
                    WHEN AVG(dewpoint_value) IS NOT NULL AND AVG(temperature_value) IS NOT NULL
                    THEN ROUND(100.0 * EXP((17.625 * AVG(dewpoint_value)) / (243.04 + AVG(dewpoint_value)))
                         / EXP((17.625 * AVG(temperature_value)) / (243.04 + AVG(temperature_value))))::BIGINT
                    ELSE NULL
                END::BIGINT AS humidity,
                -- Rain: sum precip_in where type is rain (already liquid inches)
                SUM(CASE WHEN precip_type = 'rain' THEN precip_in ELSE 0 END)
                    FILTER (WHERE precip_in IS NOT NULL AND isfinite(precip_in) AND precip_in >= 0)::DOUBLE AS rain_amt,
                -- Snow: precip_in * 10 (default snow ratio) to convert liquid equivalent to snow inches
                SUM(CASE WHEN precip_type = 'snow' THEN precip_in * 10.0 ELSE 0 END)
                    FILTER (WHERE precip_in IS NOT NULL AND isfinite(precip_in) AND precip_in >= 0)::DOUBLE AS snow_amt,
                -- Ice: liquid equivalent inches (roughly 1:1)
                SUM(CASE WHEN precip_type = 'ice' THEN precip_in ELSE 0 END)
                    FILTER (WHERE precip_in IS NOT NULL AND isfinite(precip_in) AND precip_in >= 0)::DOUBLE AS ice_amt
            FROM classified
            GROUP BY station_id
            "#,
            sql_string_list(&file_paths),
            station_filter,
            time_filter,
            utc_timestamp_sql(&start_time_expr),
            utc_timestamp_sql(&end_time_expr),
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
        self.local_daily_observations(req, station_ids, UtcOffset::UTC)
            .await
    }

    async fn local_daily_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
        days: UtcOffset,
    ) -> Result<Vec<DailyObservation>, Error> {
        let station_filter = station_filter(&station_ids)?;
        let file_params = observation_file_params(req, OffsetDateTime::now_utc());
        if empty_publication_window(&file_params) {
            return Ok(vec![]);
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
        let day = day_sql("generated_at::TIMESTAMPTZ", days);
        let precip_type = PRECIP_TYPE_SQL;
        let dedup = DEDUP_OBSERVATIONS_SQL;
        let normalized = NORMALIZE_OBSERVATIONS_SQL;
        let query_sql = format!(
            r#"
            WITH parquet_data AS (
                SELECT * FROM (
                    SELECT NULL::VARCHAR AS station_id, NULL::VARCHAR AS generated_at,
                           NULL::DOUBLE AS temperature_value, NULL::BIGINT AS wind_speed,
                           NULL::BIGINT AS wind_direction,
                           NULL::DOUBLE AS dewpoint_value, NULL::DOUBLE AS precip_in,
                           NULL::VARCHAR AS temperature_unit_code,
                           NULL::VARCHAR AS wx_string, NULL::VARCHAR AS filename,
                           NULL::VARCHAR AS dewpoint_unit_code
                    WHERE false
                    UNION ALL BY NAME
                    SELECT * FROM read_parquet([{}], union_by_name = true, filename = true)
                )
                {} {}
            ),
            deduped AS ({dedup}),
            normalized AS ({normalized}),
            -- Classify each observation's precipitation type
            classified AS (
                SELECT *, {precip_type} AS precip_type
                FROM normalized
            )
            SELECT
                station_id::VARCHAR AS station_id,
                {day} AS date,
                (MIN(temperature_value) FILTER (WHERE temperature_value IS NOT NULL))::DOUBLE AS temp_low,
                (MAX(temperature_value) FILTER (WHERE temperature_value IS NOT NULL))::DOUBLE AS temp_high,
                (MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500))::BIGINT AS wind_speed,
                MAX(temperature_unit_code)::VARCHAR AS temperature_unit_code,
                FIRST(wind_direction ORDER BY wind_speed DESC, generated_at::TIMESTAMPTZ DESC)
                    FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500)::BIGINT AS wind_direction,
                CASE
                    WHEN AVG(dewpoint_value) IS NOT NULL AND AVG(temperature_value) IS NOT NULL
                    THEN ROUND(100.0 * EXP((17.625 * AVG(dewpoint_value)) / (243.04 + AVG(dewpoint_value)))
                         / EXP((17.625 * AVG(temperature_value)) / (243.04 + AVG(temperature_value))))::BIGINT
                    ELSE NULL
                END::BIGINT AS humidity,
                SUM(CASE WHEN precip_type = 'rain' THEN precip_in ELSE 0 END)
                    FILTER (WHERE precip_in IS NOT NULL AND isfinite(precip_in) AND precip_in >= 0)::DOUBLE AS rain_amt,
                SUM(CASE WHEN precip_type = 'snow' THEN precip_in * 10.0 ELSE 0 END)
                    FILTER (WHERE precip_in IS NOT NULL AND isfinite(precip_in) AND precip_in >= 0)::DOUBLE AS snow_amt,
                SUM(CASE WHEN precip_type = 'ice' THEN precip_in ELSE 0 END)
                    FILTER (WHERE precip_in IS NOT NULL AND isfinite(precip_in) AND precip_in >= 0)::DOUBLE AS ice_amt
            FROM classified
            GROUP BY station_id, {day}
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

    async fn prepare_files(&self, stopping: &CancellationToken) -> Result<usize, Error> {
        let Some(derived) = &self.derived else {
            return Ok(0);
        };
        let now = OffsetDateTime::now_utc();
        let oldest = now - DERIVED_WINDOW;
        let mut files: Vec<ParquetFileName> = self
            .file_access
            .grab_file_names(FileParams {
                start: Some(oldest),
                end: Some(now),
                observations: Some(false),
                forecasts: Some(true),
            })
            .await?
            .iter()
            .filter_map(|name| ParquetFileName::parse(name).ok())
            .filter(|file| derived.missing(file))
            .collect();
        // Newest first: current pages read the newest files.
        files.sort_by_key(|file| std::cmp::Reverse(file.generated_at));
        let mut made = 0;
        for file in files {
            if stopping.is_cancelled() {
                break;
            }
            let _slot = self.slot().await?;
            let source = self.file_access.build_file_path(&file);
            let derived = derived.clone();
            let copied =
                tokio::task::spawn_blocking(move || derived.copy(&file, &source)).await??;
            if copied == derived::Copied::Made {
                made += 1;
            }
        }
        let derived = derived.clone();
        let oldest = oldest.to_offset(UtcOffset::UTC).date().previous_day();
        if let Some(oldest) = oldest {
            tokio::task::spawn_blocking(move || derived.prune(oldest)).await??;
        }
        if made > 0 {
            info!("derived {made} forecast files");
        }
        Ok(made)
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
        let latest_temp = doubles(batch, "latest_temp")?;
        let latest_temp_time = strings(batch, "latest_temp_time")?;
        let latest_temp_unit = strings(batch, "latest_temp_unit_code")?;
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
                latest_temp: double(latest_temp, row).map(|value| {
                    convert(
                        value,
                        &text(latest_temp_unit, row).unwrap_or_default(),
                        unit,
                    )
                }),
                latest_temp_time: text(latest_temp_time, row),
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
    /// Most recent finite temperature in the requested observation window.
    #[serde(default)]
    pub latest_temp: Option<f64>,
    /// Timestamp of the report supplying `latest_temp`, even when a newer
    /// report has no usable temperature.
    #[serde(default)]
    pub latest_temp_time: Option<String>,
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
            let publication = file_access::ParquetFileName::parse(name).unwrap();
            let publication_day = directory
                .path()
                .join(publication.generated_at.date().to_string());
            std::fs::create_dir_all(&publication_day).unwrap();
            let path = publication_day.join(name);
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
    async fn latest_temperature_keeps_its_report_time_after_a_missing_reading() {
        let directory = data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT 'KORD' AS station_id, generated_at,
                    temperature_value::DOUBLE AS temperature_value,
                    'celcius' AS temperature_unit_code
             FROM (VALUES
                 ('2026-01-17T18:00:00Z', 30.0),
                 ('2026-01-17T19:00:00Z', 20.0),
                 ('2026-01-17T20:00:00Z', NULL)
             ) AS reports(generated_at, temperature_value)",
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

        assert_eq!(kord.latest_temp, Some(68.0));
        assert_ne!(kord.latest_temp, Some(kord.temp_low));
        assert_ne!(kord.latest_temp, Some(kord.temp_high));
        assert_eq!(kord.temp_high, 86.0);
        assert_eq!(
            kord.latest_temp_time.as_deref(),
            Some("2026-01-17T19:00:00Z")
        );
        assert_eq!(kord.end_time, "2026-01-17T20:00:00.000000Z");
        assert_eq!(kord.rain_amt, None, "old files still lack precipitation");
    }

    #[tokio::test]
    async fn latest_temperature_uses_its_own_unit_and_orders_report_instants() {
        let directory = data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT 'KTEST' AS station_id, generated_at,
                    temperature_value::DOUBLE AS temperature_value,
                    temperature_unit_code
             FROM (VALUES
                 ('2026-01-17T18:00:00Z', 70.0, 'fahrenheit'),
                 ('2026-01-17T14:00:00-05:00', 10.0, 'celcius')
             ) AS reports(generated_at, temperature_value, temperature_unit_code)",
        )]);
        let request = ObservationRequest {
            station_ids: "KTEST".into(),
            ..day_request()
        };
        let observations = access(&directory)
            .observation_data(&request, request.station_ids())
            .await
            .unwrap();
        let latest = &observations[0];

        // 14:00-05:00 follows 18:00Z, despite sorting earlier as text. Its
        // Celsius reading must not inherit the other report's Fahrenheit unit.
        assert_eq!(latest.latest_temp, Some(50.0));
        assert!((latest.temp_high - 70.0).abs() < 1e-9);
        assert_eq!(latest.temp_low, 50.0);
        assert_eq!(
            latest.latest_temp_time.as_deref(),
            Some("2026-01-17T14:00:00-05:00")
        );
        assert_eq!(latest.wind_speed, None);
        assert_eq!(latest.humidity, None);
    }

    #[test]
    fn historical_serialized_observations_have_no_latest_temperature() {
        let observation: Observation = serde_json::from_value(serde_json::json!({
            "station_id": "KORD",
            "start_time": "2026-01-17T00:00:00Z",
            "end_time": "2026-01-17T18:00:00Z",
            "temp_low": 55.0,
            "temp_high": 75.0,
            "temp_unit_code": "fahrenheit"
        }))
        .unwrap();

        assert_eq!(observation.latest_temp, None);
        assert_eq!(observation.latest_temp_time, None);
    }

    #[tokio::test]
    async fn repeated_reports_count_once_and_latest_publication_corrects_them() {
        let directory = data_dir(&[
            (
                "observations_2026-01-17T18:01:00Z.parquet",
                "SELECT 'KTEST' AS station_id, '2026-01-17T18:00:00Z' AS generated_at,
                        10.0::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code,
                        0.1::DOUBLE AS precip_in, 'RA' AS wx_string",
            ),
            (
                "observations_2026-01-17T18:05:00Z.parquet",
                "SELECT 'KTEST' AS station_id, '2026-01-17T13:00:00-05:00' AS generated_at,
                        10.0::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code,
                        0.1::DOUBLE AS precip_in, 'RA' AS wx_string",
            ),
            (
                "observations_2026-01-17T18:10:00Z.parquet",
                "SELECT 'KTEST' AS station_id, '2026-01-17T18:00:00Z' AS generated_at,
                        12.0::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code,
                        0.2::DOUBLE AS precip_in, 'RA' AS wx_string",
            ),
            (
                "observations_2026-01-17T19:05:00Z.parquet",
                "SELECT station_id, '2026-01-17T19:00:00Z' AS generated_at,
                        temperature_value::DOUBLE AS temperature_value,
                        'celcius' AS temperature_unit_code, precip_in::DOUBLE AS precip_in, wx_string
                 FROM (VALUES ('KTEST', 10.0, 0.05, 'RA'),
                              ('KZERO', -2.0, 0.0, 'SN'),
                              ('KNULL', 10.0, NULL, 'RA'))
                      AS reports(station_id, temperature_value, precip_in, wx_string)",
            ),
        ]);
        let request = ObservationRequest {
            station_ids: "KTEST,KZERO,KNULL".into(),
            ..day_request()
        };
        let access = access(&directory);
        let observations = access
            .observation_data(&request, request.station_ids())
            .await
            .unwrap();
        let daily = access
            .daily_observations(&request, request.station_ids())
            .await
            .unwrap();

        for (station, rain, snow, ice) in observations
            .iter()
            .map(|o| (&o.station_id, o.rain_amt, o.snow_amt, o.ice_amt))
            .chain(
                daily
                    .iter()
                    .map(|o| (&o.station_id, o.rain_amt, o.snow_amt, o.ice_amt)),
            )
        {
            match station.as_str() {
                "KTEST" => {
                    assert_eq!(
                        rain,
                        Some(0.25),
                        "corrected 0.20 report plus distinct 0.05 report"
                    );
                    assert_eq!((snow, ice), (Some(0.0), Some(0.0)));
                }
                "KZERO" => assert_eq!((rain, snow, ice), (Some(0.0), Some(0.0), Some(0.0))),
                "KNULL" => assert_eq!((rain, snow, ice), (None, None, None)),
                _ => panic!("unexpected station {station}"),
            }
        }
        assert_eq!(observations.len(), 3);
        assert_eq!(daily.len(), 3);
        let corrected = observations
            .iter()
            .find(|o| o.station_id == "KTEST")
            .unwrap();
        assert!((corrected.temp_high - 53.6).abs() < 1e-9);
    }

    #[tokio::test]
    async fn late_publications_preserve_utc_observation_days_and_period_bounds() {
        let directory = data_dir(&[
            (
                "observations_2026-01-18T04:05:00Z.parquet",
                "SELECT 'KTEST' AS station_id, generated_at, temperature_value::DOUBLE AS temperature_value,
                        'celcius' AS temperature_unit_code
                 FROM (VALUES ('2026-01-17T18:00:00Z', 10.0),
                              ('2026-01-17T14:00:00-05:00', 20.0),
                              ('2026-01-17T23:00:00-05:00', 30.0))
                      AS reports(generated_at, temperature_value)",
            ),
            (
                "observations_2026-01-19T00:00:01Z.parquet",
                "SELECT 'KTEST' AS station_id, '2026-01-17T21:00:00Z' AS generated_at,
                        99.0::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code",
            ),
        ]);
        let mut request = ObservationRequest {
            station_ids: "KTEST".into(),
            ..day_request()
        };
        let access = access(&directory);
        let observations = access
            .observation_data(&request, request.station_ids())
            .await
            .unwrap();
        assert_eq!(
            observations.len(),
            1,
            "late publication inside 24-hour grace is included"
        );
        assert_eq!(observations[0].start_time, "2026-01-17T18:00:00.000000Z");
        assert_eq!(observations[0].end_time, "2026-01-17T19:00:00.000000Z");
        assert_eq!(
            observations[0].temp_high, 68.0,
            "reports outside the window or publication grace are excluded"
        );

        request.end = Some(time::macros::datetime!(2026-01-18 05:00 UTC));
        let daily = access
            .daily_observations(&request, request.station_ids())
            .await
            .unwrap();
        assert_eq!(daily.len(), 2);
        let next_day = daily
            .iter()
            .find(|o| o.date.starts_with("2026-01-18"))
            .unwrap();
        assert_eq!(next_day.temp_high, 86.0);
        assert_eq!(next_day.temp_low, 86.0);
    }

    #[tokio::test]
    async fn observation_end_is_inclusive_and_explicit_subsecond_cutoff_excludes_it() {
        let directory = data_dir(&[(
            "observations_2026-01-18T00:05:00Z.parquet",
            "SELECT 'KTEST' AS station_id, '2026-01-18T00:00:00Z' AS generated_at,
                    10.0::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code",
        )]);
        let mut request = ObservationRequest {
            station_ids: "KTEST".into(),
            ..day_request()
        };
        let access = access(&directory);
        assert_eq!(
            access
                .observation_data(&request, request.station_ids())
                .await
                .unwrap()
                .len(),
            1
        );
        request.end = request.end.map(|end| end - Duration::nanoseconds(1));
        assert!(
            access
                .observation_data(&request, request.station_ids())
                .await
                .unwrap()
                .is_empty()
        );
    }

    fn forecast_request() -> ForecastRequest {
        ForecastRequest {
            start: Some(time::macros::datetime!(2026-01-30 06:00 UTC)),
            end: Some(time::macros::datetime!(2026-01-30 18:00 UTC)),
            generated_start: Some(time::macros::datetime!(2026-01-17 00:00 UTC)),
            generated_end: Some(
                time::macros::datetime!(2026-01-17 18:00 UTC) - Duration::nanoseconds(1),
            ),
            station_ids: "KTEST".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        }
    }

    #[tokio::test]
    async fn forecast_discovery_uses_issue_window_and_strict_cutoff_across_offsets() {
        let directory = data_dir(&[
            (
                "forecasts_2026-01-17T18:05:00Z.parquet",
                "SELECT 'KTEST' AS station_id, '2026-01-30T02:00:00+02:00' AS begin_time,
                        '2026-01-31T02:00:00+02:00' AS end_time,
                        generated_at, 40::BIGINT AS min_temp, max_temp::BIGINT AS max_temp,
                        'fahrenheit' AS temperature_unit_code
                 FROM (VALUES ('2026-01-17T17:00:00Z', 60),
                              ('2026-01-17T12:30:00-05:00', 70),
                              ('2026-01-17T18:00:00Z', 90)) AS issues(generated_at, max_temp)",
            ),
            (
                "forecasts_2026-01-18T18:00:01Z.parquet",
                "SELECT 'KTEST' AS station_id, '2026-01-30T00:00:00Z' AS begin_time,
                        '2026-01-31T00:00:00Z' AS end_time,
                        '2026-01-17T17:50:00Z' AS generated_at, 40::BIGINT AS min_temp,
                        80::BIGINT AS max_temp, 'fahrenheit' AS temperature_unit_code",
            ),
        ]);
        let request = forecast_request();
        let forecasts = access(&directory)
            .forecasts_data(&request, request.station_ids())
            .await
            .unwrap();
        assert_eq!(
            forecasts.len(),
            1,
            "find issuance files outside the valid-day directories"
        );
        let forecast = &forecasts[0];
        assert_eq!(
            forecast.temp_high, 70,
            "newest eligible issue, ordered by instant"
        );
        assert_eq!(forecast.date, "2026-01-30 00:00:00");
        assert_eq!(forecast.start_time, "2026-01-30T06:00:00.000000Z");
        assert_eq!(forecast.end_time, "2026-01-30T18:00:00.000000Z");
        assert_eq!(
            forecast.rain_amt, None,
            "missing QPF must not become dry weather"
        );
    }

    #[tokio::test]
    async fn forecast_rain_subtracts_each_native_snow_interval_liquid_amount() {
        let directory = data_dir(&[(
            "forecasts_2026-01-17T10:05:00Z.parquet",
            "SELECT station_id, begin_time, end_time, '2026-01-17T10:00:00Z' AS generated_at,
                    30::BIGINT AS min_temp, 60::BIGINT AS max_temp, 'fahrenheit' AS temperature_unit_code,
                    qpf::DOUBLE AS liquid_precipitation_amt, snow::DOUBLE AS snow_amt, ratio::DOUBLE AS snow_ratio
             FROM (VALUES
                 ('KTEST', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', 1.0, 10.0, 10.0),
                 ('KTEST', '2026-01-17T06:00:00Z', '2026-01-17T12:00:00Z', 1.0, 10.0, 20.0),
                 ('KTEST', '2026-01-17T00:00:00Z', '2026-01-17T12:00:00Z', 2.0, 20.0, 20.0),
                 ('KZERO', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', 0.0, 0.0, NULL),
                 ('KEMPTY', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', NULL, 1.0, 20.0),
                 ('KRATIO', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', 0.25, 1.0, NULL)
             ) AS periods(station_id, begin_time, end_time, qpf, snow, ratio)",
        )]);
        let request = ForecastRequest {
            start: day_request().start,
            end: day_request().end,
            station_ids: "KTEST,KZERO,KEMPTY,KRATIO".into(),
            ..forecast_request()
        };
        let forecasts = access(&directory)
            .forecasts_data(&request, request.station_ids())
            .await
            .unwrap();
        assert_eq!(forecasts.len(), 4);
        let station = |id: &str| forecasts.iter().find(|f| f.station_id == id).unwrap();
        assert_eq!(station("KTEST").snow_amt, Some(20.0));
        assert_eq!(
            station("KTEST").rain_amt,
            Some(0.5),
            "2 inches QPF minus (10/10 + 10/20) snow liquid"
        );
        assert_eq!(station("KZERO").rain_amt, Some(0.0));
        assert_eq!(station("KEMPTY").rain_amt, None);
        assert_eq!(
            station("KRATIO").rain_amt,
            Some(0.25),
            "preserve the documented fallback when conversion is unavailable"
        );
    }

    #[test]
    fn archive_windows_are_bounded_and_never_scan_future_publications() {
        let now = time::macros::datetime!(2026-01-17 19:00 UTC);
        let request = ForecastRequest {
            generated_start: None,
            generated_end: None,
            ..forecast_request()
        };
        assert_eq!(
            forecast_generated_window(&request, now),
            (now - Duration::days(7), now)
        );
        let historical = ForecastRequest {
            end: Some(now - Duration::days(2)),
            ..request
        };
        assert_eq!(
            forecast_generated_window(&historical, now),
            (now - Duration::days(9), now - Duration::days(2))
        );
        let params = observation_file_params(&day_request(), now);
        assert_eq!(
            params.start,
            Some(time::macros::datetime!(2026-01-16 00:00 UTC))
        );
        assert_eq!(params.end, Some(now));
    }

    #[tokio::test]
    async fn wind_direction_stays_paired_with_peak_speed_even_when_missing() {
        let directory = data_dir(&[
            (
                "observations_2026-01-17T19:05:00Z.parquet",
                "SELECT station_id, generated_at, 10.0::DOUBLE AS temperature_value,
                        'celcius' AS temperature_unit_code, wind_speed::BIGINT AS wind_speed,
                        wind_direction::BIGINT AS wind_direction
                 FROM (VALUES ('KTEST', '2026-01-17T18:00:00Z', 5, 350),
                              ('KTEST', '2026-01-17T19:00:00Z', 20, 10),
                              ('KNULL', '2026-01-17T18:00:00Z', 5, 350),
                              ('KNULL', '2026-01-17T19:00:00Z', 20, NULL))
                      AS reports(station_id, generated_at, wind_speed, wind_direction)",
            ),
            (
                "forecasts_2026-01-17T10:05:00Z.parquet",
                "SELECT station_id, begin_time, end_time, '2026-01-17T10:00:00Z' AS generated_at,
                        30::BIGINT AS min_temp, 60::BIGINT AS max_temp,
                        'fahrenheit' AS temperature_unit_code, wind_speed::BIGINT AS wind_speed,
                        wind_direction::BIGINT AS wind_direction
                 FROM (VALUES ('KTEST', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', 5, 350),
                              ('KTEST', '2026-01-17T06:00:00Z', '2026-01-17T12:00:00Z', 20, 10),
                              ('KNULL', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', 5, 350),
                              ('KNULL', '2026-01-17T06:00:00Z', '2026-01-17T12:00:00Z', 20, NULL))
                      AS periods(station_id, begin_time, end_time, wind_speed, wind_direction)",
            ),
        ]);
        let observations_request = ObservationRequest {
            station_ids: "KTEST,KNULL".into(),
            ..day_request()
        };
        let forecast_request = ForecastRequest {
            start: observations_request.start,
            end: observations_request.end,
            station_ids: observations_request.station_ids.clone(),
            ..forecast_request()
        };
        let access = access(&directory);
        let observations = access
            .observation_data(&observations_request, observations_request.station_ids())
            .await
            .unwrap();
        let daily = access
            .daily_observations(&observations_request, observations_request.station_ids())
            .await
            .unwrap();
        let forecasts = access
            .forecasts_data(&forecast_request, forecast_request.station_ids())
            .await
            .unwrap();
        assert_eq!(
            (observations.len(), daily.len(), forecasts.len()),
            (2, 2, 2)
        );
        for (station, speed, direction) in observations
            .iter()
            .map(|o| (&o.station_id, o.wind_speed, o.wind_direction))
            .chain(
                daily
                    .iter()
                    .map(|o| (&o.station_id, o.wind_speed, o.wind_direction)),
            )
            .chain(
                forecasts
                    .iter()
                    .map(|f| (&f.station_id, f.wind_speed, f.wind_direction)),
            )
        {
            assert_eq!(speed, Some(20));
            assert_eq!(direction, if station == "KTEST" { Some(10) } else { None });
        }
    }

    #[tokio::test]
    async fn humidity_and_precipitation_use_normalized_temperature_units() {
        let directory = data_dir(&[(
            "observations_2026-01-17T19:05:00Z.parquet",
            "SELECT station_id, generated_at, temperature_value::DOUBLE AS temperature_value,
                    temperature_unit_code, dewpoint_value::DOUBLE AS dewpoint_value,
                    dewpoint_unit_code, precip_in::DOUBLE AS precip_in
             FROM (VALUES ('KTEST', '2026-01-17T18:00:00Z', 20.0, 'celcius', 10.0, 'celcius', NULL),
                          ('KTEST', '2026-01-17T19:00:00Z', 68.0, 'fahrenheit', 50.0, 'fahrenheit', NULL),
                          ('KSNOW', '2026-01-17T19:00:00Z', 35.0, 'fahrenheit', NULL, NULL, 0.01))
                  AS reports(station_id, generated_at, temperature_value, temperature_unit_code,
                             dewpoint_value, dewpoint_unit_code, precip_in)",
        )]);
        let request = ObservationRequest {
            station_ids: "KTEST,KSNOW".into(),
            ..day_request()
        };
        let access = access(&directory);
        let observations = access
            .observation_data(&request, request.station_ids())
            .await
            .unwrap();
        let daily = access
            .daily_observations(&request, request.station_ids())
            .await
            .unwrap();
        for (station, high, low, humidity, rain, snow) in observations
            .iter()
            .map(|o| {
                (
                    &o.station_id,
                    o.temp_high,
                    o.temp_low,
                    o.humidity,
                    o.rain_amt,
                    o.snow_amt,
                )
            })
            .chain(daily.iter().map(|o| {
                (
                    &o.station_id,
                    o.temp_high,
                    o.temp_low,
                    o.humidity,
                    o.rain_amt,
                    o.snow_amt,
                )
            }))
        {
            if station == "KTEST" {
                assert_eq!((high, low), (68.0, 68.0));
                assert_eq!(humidity, Some(53));
            } else {
                assert_eq!(rain, Some(0.0));
                assert_eq!(
                    snow,
                    Some(0.1),
                    "35 degrees Fahrenheit is below the 2 Celsius fallback threshold"
                );
            }
        }
    }

    #[tokio::test]
    async fn forecast_temperature_extrema_normalize_units_before_aggregation() {
        let directory = data_dir(&[(
            "forecasts_2026-01-17T10:05:00Z.parquet",
            "SELECT 'KTEST' AS station_id, begin_time, end_time, '2026-01-17T10:00:00Z' AS generated_at,
                    min_temp::BIGINT AS min_temp, max_temp::BIGINT AS max_temp, temperature_unit_code
             FROM (VALUES ('2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', 0, 30, 'celcius'),
                          ('2026-01-17T06:00:00Z', '2026-01-17T12:00:00Z', 32, 68, 'fahrenheit'))
                  AS periods(begin_time, end_time, min_temp, max_temp, temperature_unit_code)",
        )]);
        let mut request = ForecastRequest {
            start: day_request().start,
            end: day_request().end,
            ..forecast_request()
        };
        let access = access(&directory);
        let forecasts = access
            .forecasts_data(&request, request.station_ids())
            .await
            .unwrap();
        assert_eq!((forecasts[0].temp_high, forecasts[0].temp_low), (86, 32));
        request.temperature_unit = TemperatureUnit::Celsius;
        let forecasts = access
            .forecasts_data(&request, request.station_ids())
            .await
            .unwrap();
        assert_eq!((forecasts[0].temp_high, forecasts[0].temp_low), (30, 0));
    }

    #[tokio::test]
    async fn newest_publication_of_same_forecast_issue_wins() {
        let directory = data_dir(&[
            (
                "forecasts_2026-01-17T10:05:00Z.parquet",
                "SELECT 'KTEST' AS station_id, '2026-01-17T00:00:00Z' AS begin_time,
                        '2026-01-18T00:00:00Z' AS end_time, '2026-01-17T10:00:00Z' AS generated_at,
                        40::BIGINT AS min_temp, 90::BIGINT AS max_temp,
                        'fahrenheit' AS temperature_unit_code, 0.1::DOUBLE AS liquid_precipitation_amt",
            ),
            (
                "forecasts_2026-01-17T06:00:00-05:00.parquet",
                "SELECT 'KTEST' AS station_id, '2026-01-17T00:00:00Z' AS begin_time,
                        '2026-01-18T00:00:00Z' AS end_time, '2026-01-17T05:00:00-05:00' AS generated_at,
                        40::BIGINT AS min_temp, 65::BIGINT AS max_temp,
                        'fahrenheit' AS temperature_unit_code, 0.2::DOUBLE AS liquid_precipitation_amt",
            ),
        ]);
        let request = ForecastRequest {
            start: day_request().start,
            end: day_request().end,
            ..forecast_request()
        };
        let forecasts = access(&directory)
            .forecasts_data(&request, request.station_ids())
            .await
            .unwrap();
        assert_eq!(forecasts.len(), 1);
        assert_eq!(forecasts[0].temp_high, 65);
        assert_eq!(forecasts[0].rain_amt, Some(0.2));
    }

    struct UnexpectedFileLookup;

    #[async_trait::async_trait]
    impl FileData for UnexpectedFileLookup {
        async fn grab_file_names(&self, _: FileParams) -> Result<Vec<String>, file_access::Error> {
            panic!("a fully future publication window must not reach file listing")
        }

        fn build_file_paths(&self, _: Vec<String>) -> Vec<String> {
            panic!("no future files should be resolved")
        }

        fn build_file_path(&self, _: &file_access::ParquetFileName) -> String {
            panic!("no future files should be resolved")
        }

        async fn download_file(
            &self,
            _: &file_access::ParquetFileName,
        ) -> Result<axum::body::Body, file_access::Error> {
            panic!("no future files should be downloaded")
        }
    }

    #[tokio::test]
    async fn fully_future_publication_windows_return_empty_without_listing_files() {
        let access = WeatherAccess::new(Arc::new(UnexpectedFileLookup));
        let future = OffsetDateTime::now_utc() + Duration::days(10);
        let request = ObservationRequest {
            start: Some(future),
            end: Some(future + Duration::days(1)),
            ..day_request()
        };
        assert!(
            access
                .observation_data(&request, request.station_ids())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            access
                .daily_observations(&request, request.station_ids())
                .await
                .unwrap()
                .is_empty()
        );
        let request = ForecastRequest {
            start: Some(future),
            end: Some(future + Duration::days(1)),
            generated_start: Some(future),
            generated_end: Some(future + Duration::hours(1)),
            ..forecast_request()
        };
        assert!(
            access
                .forecasts_data(&request, request.station_ids())
                .await
                .unwrap()
                .is_empty()
        );
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

    const FORECAST_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../e2e/fixtures/weather_data/2026-01-17/forecasts_2026-01-17T17:16:19.76658783Z.parquet"
    );

    /// One period per row: station, begin, end, issue time, low, high, unit,
    /// wind speed and direction, humidity max and min, PoP, QPF, snow,
    /// snow ratio, ice.
    fn forecast_rows(rows: &str) -> String {
        format!(
            "SELECT station_id, begin_time, end_time, generated_at,
                    min_temp::BIGINT AS min_temp, max_temp::BIGINT AS max_temp, temperature_unit_code,
                    wind_speed::BIGINT AS wind_speed, wind_direction::BIGINT AS wind_direction,
                    relative_humidity_max::BIGINT AS relative_humidity_max,
                    relative_humidity_min::BIGINT AS relative_humidity_min,
                    pop::BIGINT AS twelve_hour_probability_of_precipitation,
                    qpf::DOUBLE AS liquid_precipitation_amt, snow::DOUBLE AS snow_amt,
                    ratio::DOUBLE AS snow_ratio, ice::DOUBLE AS ice_amt
             FROM (VALUES {rows}) AS periods(station_id, begin_time, end_time, generated_at,
                  min_temp, max_temp, temperature_unit_code, wind_speed, wind_direction,
                  relative_humidity_max, relative_humidity_min, pop, qpf, snow, ratio, ice)"
        )
    }

    /// The real January forecast file (an older schema without snow, ice or
    /// snow ratio) plus files covering what the query distinguishes:
    /// repeated issues and publications of one period, equal issue times,
    /// UTC offsets in every timestamp, late publications, a file without
    /// most columns, unit spellings, and precipitation at several interval
    /// lengths with gaps, zeros, negatives and missing values.
    fn forecast_data_dir() -> tempfile::TempDir {
        let first = forecast_rows(
            "('KTEST', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', '2026-01-17T10:00:00Z', 30, 40, 'fahrenheit', 5, 350, 90, 60, 20, 0.1, 1.0, 10.0, 0.0),
             ('KTEST', '2026-01-17T06:00:00Z', '2026-01-17T12:00:00Z', '2026-01-17T10:00:00Z', 31, 45, 'fahrenheit', 12, 10, 80, 50, 30, 0.2, 0.0, 10.0, 0.01),
             ('KTEST', '2026-01-17T12:00:00Z', '2026-01-17T18:00:00Z', '2026-01-17T10:00:00Z', 33, 48, 'fahrenheit', 12, 20, 85, 55, 40, -0.1, 2.0, NULL, NULL),
             ('KTEST', '2026-01-17T00:00:00Z', '2026-01-17T12:00:00Z', '2026-01-17T10:00:00Z', 30, 45, 'fahrenheit', NULL, NULL, NULL, NULL, 50, 0.3, 1.0, 20.0, 0.0),
             ('KTEST', '2026-01-17T12:00:00Z', '2026-01-18T00:00:00Z', '2026-01-17T10:00:00Z', 29, 47, 'fahrenheit', 8, 180, 95, 40, 60, 0.4, 3.0, 15.0, 0.02),
             ('KTEST', '2026-01-18T00:00:00-05:00', '2026-01-18T06:00:00-05:00', '2026-01-17T10:00:00Z', -2, 3, 'celcius', 20, 270, 70, 30, 10, 0.05, NULL, NULL, NULL),
             ('KTEST', '2026-01-18T11:00:00Z', '2026-01-18T12:00:00Z', '2026-01-17T10:00:00Z', 20, 25, 'Celsius', 25, 90, 101, -1, NULL, 0.01, 0.1, 10.0, 0.0),
             ('KTEST', '2026-01-18T13:00:00Z', '2026-01-18T14:00:00Z', '2026-01-17T10:00:00Z', 20, 26, 'kelvin', 25, 100, 60, 20, NULL, 0.02, 0.2, 5.0, 0.0),
             ('KTEST', '2026-01-18T14:00:00Z', '2026-01-18T15:00:00Z', '2026-01-17T10:00:00Z', 250, 300, 'fahrenheit', 600, 45, 60, 20, NULL, 0.03, 0.0, 0.0, 0.0),
             ('KZERO', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', '2026-01-17T10:00:00Z', 30, 60, 'fahrenheit', 0, 0, 50, 50, 0, 0.0, 0.0, NULL, NULL),
             ('KEMPTY', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', '2026-01-17T10:00:00Z', 30, 60, 'fahrenheit', NULL, NULL, NULL, NULL, NULL, NULL, 1.0, 20.0, NULL),
             ('KRATIO', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', '2026-01-17T10:00:00Z', 30, 60, 'fahrenheit', 3, 30, 70, 60, 5, 0.25, 1.0, NULL, NULL),
             (NULL, '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', '2026-01-17T10:00:00Z', 30, 60, 'fahrenheit', 3, 30, 70, 60, 5, 0.25, 1.0, NULL, NULL)",
        );
        // The same instant as the first file's issue, published later, with
        // other values; and an exact duplicate period with other values.
        let republished = forecast_rows(
            "('KTEST', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', '2026-01-17T05:00:00-05:00', 28, 42, 'fahrenheit', 7, 340, 90, 60, 25, 0.15, 1.5, 12.0, 0.0),
             ('KTEST', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', '2026-01-17T05:00:00-05:00', 28, 43, 'fahrenheit', 7, 340, 90, 60, 25, 0.15, 1.5, 12.0, 0.0),
             ('KZERO', '2026-01-17T06:00:00Z', '2026-01-17T12:00:00Z', '2026-01-17T05:00:00-05:00', 30, 61, 'fahrenheit', 2, 90, 50, 50, 0, 0.0, 0.0, NULL, NULL)",
        );
        // Several issues in one file, one after the usual cutoff.
        let issues = forecast_rows(
            "('KTEST', '2026-01-17T12:00:00Z', '2026-01-18T00:00:00Z', '2026-01-17T17:00:00Z', 29, 60, 'fahrenheit', 9, 200, 95, 40, 60, 0.5, 3.0, 15.0, 0.02),
             ('KTEST', '2026-01-17T12:00:00Z', '2026-01-18T00:00:00Z', '2026-01-17T12:30:00-05:00', 29, 70, 'fahrenheit', 9, 210, 95, 40, 60, 0.6, 3.0, 15.0, 0.02),
             ('KTEST', '2026-01-17T12:00:00Z', '2026-01-18T00:00:00Z', '2026-01-17T18:00:00Z', 29, 90, 'fahrenheit', 9, 220, 95, 40, 60, 0.7, 3.0, 15.0, 0.02),
             ('KORD', '2026-01-18T00:00:00-06:00', '2026-01-18T12:00:00-06:00', '2026-01-17T17:30:00Z', 10, 20, 'fahrenheit', 15, 300, 80, 60, 70, 0.3, 2.5, 12.0, 0.0)",
        );
        // Published a day and a second after its issue: outside the grace.
        let late = forecast_rows(
            "('KTEST', '2026-01-17T00:00:00Z', '2026-01-18T00:00:00Z', '2026-01-17T17:50:00Z', 20, 99, 'fahrenheit', 1, 1, 1, 1, 1, 9.0, 9.0, 1.0, 9.0)",
        );
        let directory = data_dir(&[
            ("forecasts_2026-01-17T10:05:00Z.parquet", &first),
            ("forecasts_2026-01-17T06:00:00-05:00.parquet", &republished),
            ("forecasts_2026-01-17T18:05:00Z.parquet", &issues),
            ("forecasts_2026-01-18T18:00:01Z.parquet", &late),
            // A file from before most columns existed.
            (
                "forecasts_2026-01-16T12:00:00Z.parquet",
                "SELECT 'KTEST' AS station_id, '2026-01-17T00:00:00Z' AS begin_time,
                        '2026-01-17T06:00:00Z' AS end_time, '2026-01-16T11:00:00Z' AS generated_at,
                        25::BIGINT AS min_temp, 50::BIGINT AS max_temp, 'fahrenheit' AS temperature_unit_code",
            ),
        ]);
        // Keep the legacy-schema sample representative and small: the old
        // all-stations query exceeds the production memory limit by design.
        // Full real-data timing and equivalence live in the ignored benchmark.
        let fixture = directory
            .path()
            .join("2026-01-17/forecasts_2026-01-17T17:16:19.76658783Z.parquet");
        open_connection().unwrap().execute_batch(&format!(
            "COPY (SELECT * FROM read_parquet('{}') WHERE station_id IN ('KORD', 'KSAW', 'KDEN')) TO '{}' (FORMAT PARQUET)",
            FORECAST_FIXTURE.replace('\'', "''"), fixture.to_string_lossy().replace('\'', "''")
        )).unwrap();
        directory
    }

    fn derived_access(directory: &tempfile::TempDir) -> WeatherAccess {
        WeatherAccess::with_derived_forecasts(
            Arc::new(crate::file_access::FileAccess::new(
                directory.path().to_string_lossy().into_owned(),
            )),
            &directory.path().join("derived"),
        )
    }

    /// Copies every forecast file, whatever its age.
    async fn derive_all(access: &WeatherAccess) -> Vec<(String, derived::Copied)> {
        let derived = access.derived.clone().unwrap();
        let names = access
            .file_access
            .grab_file_names(FileParams {
                start: None,
                end: None,
                observations: Some(false),
                forecasts: Some(true),
            })
            .await
            .unwrap();
        let mut copied = vec![];
        for name in names {
            let file = ParquetFileName::parse(&name).unwrap();
            let source = access.file_access.build_file_path(&file);
            let derived = derived.clone();
            let result = tokio::task::spawn_blocking(move || derived.copy(&file, &source))
                .await
                .unwrap()
                .unwrap();
            copied.push((name, result));
        }
        copied
    }

    fn sorted(mut forecasts: Vec<Forecast>) -> Vec<Forecast> {
        forecasts.sort_by(|a, b| (&a.station_id, &a.date).cmp(&(&b.station_id, &b.date)));
        forecasts
    }

    /// Equal field by field; sums of precipitation may differ in the last
    /// bits because rows are added in another order.
    fn assert_same_forecasts(expected: &[Forecast], actual: &[Forecast], context: &str) {
        assert_eq!(expected.len(), actual.len(), "row count: {context}");
        let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => (a - b).abs() <= 1e-9,
            (a, b) => a == b,
        };
        for (expected, actual) in expected.iter().zip(actual) {
            let same = expected.station_id == actual.station_id
                && expected.date == actual.date
                && expected.start_time == actual.start_time
                && expected.end_time == actual.end_time
                && expected.temp_low == actual.temp_low
                && expected.temp_high == actual.temp_high
                && expected.wind_speed == actual.wind_speed
                && expected.wind_direction == actual.wind_direction
                && expected.humidity_max == actual.humidity_max
                && expected.humidity_min == actual.humidity_min
                && expected.temp_unit_code == actual.temp_unit_code
                && expected.precip_chance == actual.precip_chance
                && close(expected.rain_amt, actual.rain_amt)
                && close(expected.snow_amt, actual.snow_amt)
                && close(expected.ice_amt, actual.ice_amt);
            assert!(
                same,
                "{context}\nexpected {expected:?}\nactual   {actual:?}"
            );
        }
    }

    fn fixture_forecast_requests() -> Vec<(ForecastRequest, Vec<String>)> {
        let at = |text: &str| OffsetDateTime::parse(text, &Rfc3339).unwrap();
        let stations: Vec<String> = ["KTEST", "KZERO", "KEMPTY", "KRATIO", "KORD", "KSAW", "KDEN"]
            .map(String::from)
            .to_vec();
        let windows = [
            // A day's forecast from the previous day's issues.
            (
                "2026-01-17T00:00:00Z",
                "2026-01-18T00:00:00Z",
                Some(("2026-01-16T00:00:00Z", "2026-01-17T17:59:59.999999999Z")),
            ),
            (
                "2026-01-18T00:00:00Z",
                "2026-01-19T00:00:00Z",
                Some(("2026-01-17T00:00:00Z", "2026-01-17T23:59:59.999999999Z")),
            ),
            // A week ahead with the default issue window.
            ("2026-01-17T00:00:00Z", "2026-01-24T00:00:00Z", None),
            // Part of a day, bounds with offsets.
            (
                "2026-01-17T01:00:00-05:00",
                "2026-01-17T13:00:00-05:00",
                Some(("2026-01-17T00:00:00Z", "2026-01-17T18:00:00Z")),
            ),
            // Everything.
            (
                "2026-01-10T00:00:00Z",
                "2026-01-26T00:00:00Z",
                Some(("2026-01-10T00:00:00Z", "2026-01-19T00:00:00Z")),
            ),
        ];
        let mut requests = vec![];
        for (start, end, generated) in windows {
            for (unit, station_ids) in [
                (TemperatureUnit::Fahrenheit, stations.clone()),
                (TemperatureUnit::Celsius, stations[..2].to_vec()),
            ] {
                requests.push((
                    ForecastRequest {
                        start: Some(at(start)),
                        end: Some(at(end)),
                        generated_start: generated.map(|(start, _)| at(start)),
                        generated_end: generated.map(|(_, end)| at(end)),
                        station_ids: station_ids.join(","),
                        temperature_unit: unit,
                    },
                    station_ids,
                ));
            }
        }
        // Every station in the files, as the dashboard's fallback asks.
        requests.push((
            ForecastRequest {
                start: Some(at("2026-01-18T00:00:00Z")),
                end: Some(at("2026-01-19T00:00:00Z")),
                generated_start: Some(at("2026-01-17T00:00:00Z")),
                generated_end: Some(at("2026-01-18T00:00:00Z")),
                station_ids: String::new(),
                temperature_unit: TemperatureUnit::Fahrenheit,
            },
            vec![],
        ));
        requests
    }

    #[tokio::test]
    async fn forecasts_match_the_previous_query_with_and_without_derived_files() {
        let directory = forecast_data_dir();
        let published = access(&directory);
        let derived = derived_access(&directory);
        let copied = derive_all(&derived).await;
        assert_eq!(copied.len(), 6);
        assert!(
            copied
                .iter()
                .all(|(_, copied)| *copied == derived::Copied::Made)
        );
        assert_eq!(
            derived
                .forecast_files(copied.iter().map(|(name, _)| name.clone()).collect())
                .source,
            Vec::<String>::new(),
            "every file is read from its copy"
        );

        for (request, station_ids) in fixture_forecast_requests() {
            let context = format!(
                "{:?}..{:?} issued {:?}..{:?} for {:?}",
                request.start,
                request.end,
                request.generated_start,
                request.generated_end,
                station_ids
            );
            let expected = sorted(
                legacy::forecasts_data(&published, &request, station_ids.clone())
                    .await
                    .unwrap(),
            );
            assert!(!expected.is_empty(), "{context}");
            let from_files = published
                .forecasts_data(&request, station_ids.clone())
                .await
                .unwrap();
            assert_same_forecasts(
                &expected,
                &from_files,
                &format!("published files: {context}"),
            );
            let from_copies = derived
                .forecasts_data(&request, station_ids.clone())
                .await
                .unwrap();
            assert_same_forecasts(
                &expected,
                &from_copies,
                &format!("derived files: {context}"),
            );
        }
    }

    #[tokio::test]
    async fn unreadable_forecast_files_are_read_directly_and_fail_like_before() {
        let bad = forecast_rows(
            "('KBAD', 'not a time', '2026-01-17T06:00:00Z', '2026-01-17T10:00:00Z', 30, 40, 'fahrenheit', 5, 350, 90, 60, 20, 0.1, 1.0, 10.0, 0.0),
             ('KTEST', '2026-01-17T00:00:00Z', '2026-01-17T06:00:00Z', '2026-01-17T10:00:00Z', 30, 41, 'fahrenheit', 5, 350, 90, 60, 20, 0.1, 1.0, 10.0, 0.0)",
        );
        let directory = data_dir(&[("forecasts_2026-01-17T10:05:00Z.parquet", &bad)]);
        let published = access(&directory);
        let derived = derived_access(&directory);
        let copied = derive_all(&derived).await;
        assert_eq!(copied[0].1, derived::Copied::Refused);
        let file = ParquetFileName::parse(&copied[0].0).unwrap();
        let copies = derived.derived.as_ref().unwrap();
        assert!(copies.existing(&file).is_none());
        assert!(!copies.missing(&file), "a refused file is not copied again");

        let at = |text: &str| OffsetDateTime::parse(text, &Rfc3339).unwrap();
        let mut request = ForecastRequest {
            start: Some(at("2026-01-17T00:00:00Z")),
            end: Some(at("2026-01-18T00:00:00Z")),
            generated_start: Some(at("2026-01-16T00:00:00Z")),
            generated_end: Some(at("2026-01-17T12:00:00Z")),
            station_ids: "KTEST".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let expected = legacy::forecasts_data(&published, &request, request.station_ids())
            .await
            .unwrap();
        let actual = derived
            .forecasts_data(&request, request.station_ids())
            .await
            .unwrap();
        assert_same_forecasts(&expected, &actual, "other stations still load");
        assert_eq!(actual[0].temp_high, 41);

        request.station_ids = "KBAD".into();
        assert!(
            legacy::forecasts_data(&published, &request, request.station_ids())
                .await
                .is_err()
        );
        assert!(
            derived
                .forecasts_data(&request, request.station_ids())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn local_days_group_forecasts_and_observations_by_the_readers_calendar() {
        let periods = forecast_rows(
            "('KTEST', '2026-01-17T02:00:00Z', '2026-01-17T03:00:00Z', '2026-01-16T12:00:00Z', 40, 50, 'fahrenheit', 5, 90, 80, 60, NULL, 0.1, NULL, NULL, NULL),
             ('KTEST', '2026-01-17T06:00:00Z', '2026-01-17T07:00:00Z', '2026-01-16T12:00:00Z', 41, 60, 'fahrenheit', 6, 180, 80, 60, NULL, 0.1, NULL, NULL, NULL),
             ('KTEST', '2026-01-18T02:00:00Z', '2026-01-18T03:00:00Z', '2026-01-16T12:00:00Z', 42, 70, 'fahrenheit', 7, 270, 80, 60, NULL, 0.1, NULL, NULL, NULL)",
        );
        let directory = data_dir(&[
            ("forecasts_2026-01-16T12:05:00Z.parquet", &periods),
            (
                "observations_2026-01-17T07:00:00Z.parquet",
                "SELECT 'KTEST' AS station_id, generated_at, temperature_value::DOUBLE AS temperature_value,
                        'fahrenheit' AS temperature_unit_code
                 FROM (VALUES ('2026-01-17T02:00:00Z', 50.0), ('2026-01-17T06:00:00Z', 59.0))
                      AS reports(generated_at, temperature_value)",
            ),
        ]);
        let access = derived_access(&directory);
        derive_all(&access).await;
        let eastern = UtcOffset::from_hms(-4, 0, 0).unwrap();
        let at = |text: &str| OffsetDateTime::parse(text, &Rfc3339).unwrap();
        let request = ForecastRequest {
            start: Some(at("2026-01-16T00:00:00-04:00")),
            end: Some(at("2026-01-18T00:00:00-04:00")),
            generated_start: Some(at("2026-01-10T00:00:00Z")),
            generated_end: Some(at("2026-01-17T00:00:00Z")),
            station_ids: "KTEST".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let days = |forecasts: Vec<Forecast>| -> Vec<(String, i64, Option<f64>)> {
            sorted(forecasts)
                .into_iter()
                .map(|forecast| (forecast.date, forecast.temp_high, forecast.rain_amt))
                .collect()
        };
        assert_eq!(
            days(
                access
                    .forecasts_data(&request, request.station_ids())
                    .await
                    .unwrap()
            ),
            vec![
                ("2026-01-17 00:00:00".to_owned(), 60, Some(0.2)),
                ("2026-01-18 00:00:00".to_owned(), 70, Some(0.1)),
            ]
        );
        let local = days(
            access
                .local_forecasts(&request, request.station_ids(), eastern)
                .await
                .unwrap(),
        );
        assert_eq!(local[0], ("2026-01-16 00:00:00".to_owned(), 50, Some(0.1)));
        assert_eq!(local[1].0, "2026-01-17 00:00:00");
        assert_eq!(local[1].1, 70);
        assert!((local[1].2.unwrap() - 0.2).abs() < 1e-9);

        let observations = ObservationRequest {
            start: Some(at("2026-01-16T00:00:00-04:00")),
            end: Some(at("2026-01-18T00:00:00-04:00")),
            station_ids: "KTEST".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let mut daily = access
            .local_daily_observations(&observations, observations.station_ids(), eastern)
            .await
            .unwrap();
        daily.sort_by(|a, b| a.date.cmp(&b.date));
        let daily: Vec<(&str, f64)> = daily
            .iter()
            .map(|day| (day.date.as_str(), day.temp_high))
            .collect();
        assert_eq!(
            daily,
            vec![("2026-01-16 00:00:00", 50.0), ("2026-01-17 00:00:00", 59.0)]
        );
    }

    #[tokio::test]
    async fn derived_copies_are_made_once_and_old_ones_pruned() {
        let directory = forecast_data_dir();
        let access = derived_access(&directory);
        let root = directory.path().join("derived");
        let old_version = root.join("forecasts-v0/2026-01-17");
        std::fs::create_dir_all(&old_version).unwrap();
        derive_all(&access).await;
        let copies = root.join(super::derived::VERSION);
        assert!(copies.join("2026-01-17").is_dir());

        // Files this old are outside the window prepare_files copies, and
        // their copies are pruned.
        assert_eq!(
            access
                .prepare_files(&CancellationToken::new())
                .await
                .unwrap(),
            0
        );
        assert!(!root.join("forecasts-v0").exists());
        assert!(!copies.join("2026-01-17").exists());
        assert!(!copies.join("2026-01-16").exists());
        // Queries fall back to the published files.
        let request = &fixture_forecast_requests()[0];
        assert!(
            !access
                .forecasts_data(&request.0, request.1.clone())
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Old and new forecast queries on a copy of real data, with timings:
    ///
    /// ```text
    /// ORACLE_PERF_DATA=<weather dir> cargo test -p oracle real_forecasts -- --ignored --nocapture
    /// ```
    ///
    /// Copies are made in `<weather dir>/derived` on the first run.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs ORACLE_PERF_DATA"]
    async fn real_forecasts_match_the_previous_query() {
        let Ok(directory) = std::env::var("ORACLE_PERF_DATA") else {
            eprintln!("ORACLE_PERF_DATA is not set");
            return;
        };
        let file_access = Arc::new(crate::file_access::FileAccess::new(directory.clone()));
        let published = WeatherAccess::new(file_access.clone());
        let derived = WeatherAccess::with_derived_forecasts(
            file_access.clone(),
            &Path::new(&directory).join("derived"),
        );
        let names = file_access
            .grab_file_names(FileParams {
                start: None,
                end: None,
                observations: Some(false),
                forecasts: Some(true),
            })
            .await
            .unwrap();
        let newest = names
            .iter()
            .filter_map(|name| ParquetFileName::parse(name).ok())
            .map(|file| file.generated_at)
            .max()
            .unwrap();
        let copies = derived.derived.clone().unwrap();
        let started = std::time::Instant::now();
        let mut made = 0;
        for name in &names {
            let file = ParquetFileName::parse(name).unwrap();
            if copies.missing(&file) {
                let source = file_access.build_file_path(&file);
                let copies = copies.clone();
                tokio::task::spawn_blocking(move || copies.copy(&file, &source))
                    .await
                    .unwrap()
                    .unwrap();
                made += 1;
            }
        }
        println!(
            "\nderived {made} of {} forecast files in {:.1} s",
            names.len(),
            started.elapsed().as_secs_f64()
        );

        let now = newest;
        let today = now.replace_time(time::Time::MIDNIGHT);
        let airports: Vec<String> = "KATL,KLAX,KORD,KDFW,KDEN,KJFK,KSFO,KSEA,KLAS,KMCO,KEWR,KMIA,KPHX,KIAH,KBOS,KMSP,KFLL,KDTW,KPHL,KLGA,KBWI,KSLC,KDCA,KSAN,KTPA,KPDX,KSTL,KHNL,KBNA,KAUS,KMCI,KRDU,KMKE,KSMF,KCLT,KPIT,KSAT,KOAK,KCLE,KSJC,KIND,KCVG,KCMH,KJAN,KRSW,KABQ,KANC,KOMA,KBUF,KPBI,KBDL,KPVD,KBTV,KPWM,KMHT,KBOI,KBIL,KFSD,KFAR,KGEG,KICT,KLIT,KLEX,KBHM,KMEM,KJAX,KCHS,KRIC,KORF,KCRW,KPNS,KMOB,KSHV,KMSY,KTUL,KELP,KTUS,KCOS,KGRR,KDSM,KMSN,KDLH,KBZN,KGJT,KRAP,KFCA,KCYS,KJAR,KSGF,KFSM"
            .split(',')
            .map(String::from)
            .collect();
        let three: Vec<String> = ["KPWM", "KBTV", "KBED"].map(String::from).to_vec();
        let one = vec!["KSAW".to_owned()];
        let request = |start: OffsetDateTime,
                       end: OffsetDateTime,
                       generated: Option<(OffsetDateTime, OffsetDateTime)>,
                       station_ids: &[String]| {
            (
                ForecastRequest {
                    start: Some(start),
                    end: Some(end),
                    generated_start: generated.map(|(start, _)| start),
                    generated_end: generated.map(|(_, end)| end),
                    station_ids: station_ids.join(","),
                    temperature_unit: TemperatureUnit::Fahrenheit,
                },
                station_ids.to_vec(),
            )
        };
        let before = |time: OffsetDateTime| time - Duration::nanoseconds(1);
        let finished = today - Duration::hours(21);
        let cases = [
            (
                "dashboard: 90 airports, today from yesterday's issues",
                request(
                    today,
                    today + Duration::days(1),
                    Some((today - Duration::days(1), before(today))),
                    &airports,
                ),
            ),
            (
                "map popup: next 7 days",
                request(today, today + Duration::days(7), None, &one),
            ),
            (
                "map popup: last 7 days",
                request(
                    today - Duration::days(7),
                    today,
                    Some((today - Duration::days(8), now)),
                    &one,
                ),
            ),
            (
                "API: entry form, 3 stations, next 2 days",
                request(now, now + Duration::days(2), None, &three),
            ),
            (
                "API: leaderboard, 3 stations, finished window",
                request(finished, finished + Duration::days(1), None, &three),
            ),
            (
                "scoring: 3 stations, issues before the window",
                request(
                    finished,
                    finished + Duration::days(1),
                    Some((finished - Duration::days(7), before(finished))),
                    &three,
                ),
            ),
        ];
        println!(
            "\n| Query | Before (ms) | After, published files (ms) | After, derived files (ms) | Rows |"
        );
        println!("| --- | ---: | ---: | ---: | ---: |");
        for (name, (request, station_ids)) in cases {
            let started = std::time::Instant::now();
            let expected =
                match legacy::forecasts_data(&published, &request, station_ids.clone()).await {
                    Ok(rows) => Some(sorted(rows)),
                    Err(error) if error.to_string().contains("Out of Memory") => None,
                    Err(error) => panic!("legacy query {name}: {error}"),
                };
            let before_ms = if expected.is_some() {
                format!("{:.0}", started.elapsed().as_secs_f64() * 1000.0)
            } else {
                "OOM (512 MB)".to_string()
            };
            let started = std::time::Instant::now();
            let from_files = published
                .forecasts_data(&request, station_ids.clone())
                .await
                .unwrap();
            let files_ms = started.elapsed().as_secs_f64() * 1000.0;
            let mut copies_ms = f64::INFINITY;
            let mut from_copies = vec![];
            for _ in 0..3 {
                let started = std::time::Instant::now();
                from_copies = derived
                    .forecasts_data(&request, station_ids.clone())
                    .await
                    .unwrap();
                copies_ms = copies_ms.min(started.elapsed().as_secs_f64() * 1000.0);
            }
            if let Some(expected) = &expected {
                assert_same_forecasts(expected, &from_files, name);
                assert_same_forecasts(expected, &from_copies, name);
            } else {
                assert_same_forecasts(&from_files, &from_copies, name);
            }
            println!(
                "| {name} | {before_ms} | {files_ms:.0} | {copies_ms:.0} | {} |",
                from_copies.len()
            );
        }
    }
}
