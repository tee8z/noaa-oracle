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
    calendar::Calendar,
    file_access::{self, FileData, FileParams, ParquetFileName},
    routes::{ForecastRequest, ObservationRequest, TemperatureUnit},
};
use async_trait::async_trait;
use duckdb::{
    Connection,
    arrow::array::{Array, Float64Array, Int64Array, RecordBatch, StringArray},
};
use log::debug;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::Instant,
};
use time::{Duration, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;

mod coverage;
mod derived;
mod eligibility;
mod folds;
mod forecast_availability;
mod forecast_display_quality;
mod forecast_quality;
#[cfg(test)]
mod legacy;
mod precipitation;
mod shef;

pub use derived::DerivedForecasts;
pub use eligibility::{
    DEFAULT_DAYS, DEFAULT_WINDOW_HOURS, Eligibility, EligibleStation, MAX_DAYS, MAX_WINDOW_HOURS,
    PRECOMPUTED_DAYS,
};
pub use folds::Folds;
pub use forecast_display_quality::{ForecastQuality, ForecastRangeIssue};

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
    SELECT * EXCLUDE (filename),
        regexp_extract(filename, '(^|/)observations_([^/]+)\.parquet$', 2)::TIMESTAMPTZ AS publication_time,
        COUNT(DISTINCT STRUCT_PACK(
            temp := temperature_value, unit := temperature_unit_code,
            dew := dewpoint_value, dew_unit := dewpoint_unit_code,
            wind := wind_speed, wind_unit := wind_speed_unit_code, direction := wind_direction, direction_unit := wind_direction_unit_code,
            precip := precip_in, precip_unit := precip_unit_code, weather := wx_string,
            quality := quality_status, raw := raw_text, version := validation_version, reason := quality_reason, report_type := metar_type,
            affected := quality_metrics
        )) OVER (PARTITION BY station_id, generated_at::TIMESTAMPTZ, filename) > 1 AS publication_conflict
    FROM (
        -- Snapshots repeat earlier reports. Only the newest publication can
        -- contribute a report, so discard older publications before checking
        -- conflicts. Keep every row of that publication: ROW_NUMBER here
        -- would hide contradictory reports and incorrectly clear their QC.
        SELECT * FROM parquet_data
        QUALIFY DENSE_RANK() OVER (
            PARTITION BY station_id, generated_at::TIMESTAMPTZ
            ORDER BY regexp_extract(filename, '(^|/)observations_([^/]+)\.parquet$', 2)::TIMESTAMPTZ DESC,
                     filename DESC
        ) = 1
    ) AS latest_publications
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
// Broad screening bounds bracket the WMO air-temperature records. They flag
// suspect input for review; values inside these bounds are not proof of accuracy.
// Never clamp or replace a rejected measurement. Retain its row for quality counts,
// including windows containing no usable temperatures.
const NORMALIZE_OBSERVATIONS_SQL: &str = r#"
    WITH converted AS (
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
        'celsius'::VARCHAR AS temperature_unit_code,
        precip_in AS source_precip_in
        , (temperature_value IS NULL OR
            (NOT isfinite(temperature_value) OR lower(temperature_unit_code) NOT IN ('fahrenheit', 'celsius', 'celcius')
             OR temperature_unit_code IS NULL)) AS invalid_temperature,
        (dewpoint_value IS NOT NULL AND
            (NOT isfinite(dewpoint_value) OR (COALESCE(quality_status = 'validated', false) AND dewpoint_unit_code IS NULL)
             OR lower(COALESCE(dewpoint_unit_code, temperature_unit_code)) NOT IN ('fahrenheit', 'celsius', 'celcius')
             OR COALESCE(dewpoint_unit_code, temperature_unit_code) IS NULL)) AS invalid_dewpoint
    FROM deduped
    ), contextual AS (
        SELECT *, LAG(temperature_value) OVER station_reports AS previous_temperature,
            LAG(generated_at::TIMESTAMPTZ) OVER station_reports AS previous_time
        FROM converted
        WINDOW station_reports AS (PARTITION BY station_id ORDER BY generated_at::TIMESTAMPTZ)
    ), screened AS (
        SELECT *,
            (publication_conflict OR invalid_temperature OR invalid_dewpoint
             OR (wind_speed IS NOT NULL AND (COALESCE(quality_status = 'validated', false) OR wind_speed_unit_code IS NOT NULL)
                 AND (wind_speed_unit_code IS NULL OR lower(wind_speed_unit_code) != 'knots'))
             OR (wind_direction IS NOT NULL AND (COALESCE(quality_status = 'validated', false) OR wind_direction_unit_code IS NOT NULL)
                 AND (wind_direction_unit_code IS NULL OR lower(wind_direction_unit_code) NOT IN ('degrees true', 'degrees')))
             OR (precip_in IS NOT NULL AND (COALESCE(quality_status = 'validated', false) OR precip_unit_code IS NOT NULL)
                 AND (precip_unit_code IS NULL OR lower(precip_unit_code) != 'inches'))
             OR COALESCE(temperature_value < -90 OR temperature_value > 57, false)
             OR COALESCE(dewpoint_value < -100 OR dewpoint_value > 57, false)
             OR COALESCE(dewpoint_value > temperature_value + 0.5 + 1e-6, false)
             OR COALESCE(wind_speed < 0 OR wind_speed > 500, false)
             OR COALESCE(wind_direction < 0 OR wind_direction > 360, false)
             OR COALESCE(NOT isfinite(precip_in) OR precip_in < 0 OR precip_in > 5, false)
             OR COALESCE(ABS(temperature_value - previous_temperature) >= 15
                AND generated_at::TIMESTAMPTZ - previous_time <= INTERVAL '2 hours', false)) AS qc_screened,
            (quality_status IS NOT NULL AND quality_status NOT IN ('validated', 'unverified')) AS qc_source_rejected,
            (quality_status IS DISTINCT FROM 'validated'
             OR validation_version IS DISTINCT FROM 'metar-consistency-v1'
             OR COALESCE(trim(quality_reason) != '', false)
             OR raw_text IS NULL OR trim(raw_text) = '') AS qc_unverified
        FROM contextual
    ), assessed AS (
        SELECT * EXCLUDE (qc_source_rejected), qc_source_rejected OR qc_screened AS qc_rejected,
            -- Metric groups a flagged report's problems affect. The daemon
            -- tags its own problems in quality_metrics; the oracle's
            -- screening, missing provenance, and untagged or unknown tags
            -- affect every group.
            CASE
                WHEN NOT (qc_source_rejected OR qc_screened OR qc_unverified) THEN []::VARCHAR[]
                WHEN qc_screened OR quality_metrics IS NULL
                    OR validation_version IS DISTINCT FROM 'metar-consistency-v1'
                    OR raw_text IS NULL OR trim(raw_text) = ''
                    OR NOT list_has_all(['temperature', 'dewpoint', 'wind', 'precipitation', 'present_weather'],
                                        string_split(quality_metrics, ','))
                    THEN ['temperature', 'dewpoint', 'wind', 'precipitation', 'present_weather']
                ELSE string_split(quality_metrics, ',')
            END AS qc_groups
        FROM screened
    ), usable AS (
        -- Values of groups a rejection or unverified check affects, or of
        -- any screened report. Settlement treats them as missing. Legacy
        -- reports keep values; their counts hold settlement instead.
        SELECT *,
            NOT qc_screened AND NOT (quality_status IS NOT NULL AND qc_unverified
                AND list_contains(qc_groups, 'temperature')) AS temperature_usable,
            NOT qc_screened AND NOT (quality_status IS NOT NULL AND qc_unverified
                AND list_contains(qc_groups, 'dewpoint')) AS dewpoint_usable,
            NOT qc_screened AND NOT (quality_status IS NOT NULL AND qc_unverified
                AND list_contains(qc_groups, 'wind')) AS wind_usable,
            NOT qc_screened AND NOT (quality_status IS NOT NULL AND qc_unverified
                AND list_contains(qc_groups, 'precipitation')) AS precip_usable
        FROM assessed
    )
    SELECT * EXCLUDE (temperature_value, dewpoint_value, wind_speed, wind_direction, precip_in,
                      temperature_usable, dewpoint_usable, wind_usable, precip_usable),
        CASE WHEN temperature_usable THEN temperature_value END AS temperature_value,
        CASE WHEN dewpoint_usable THEN dewpoint_value END AS dewpoint_value,
        CASE WHEN wind_usable THEN wind_speed END AS wind_speed,
        CASE WHEN wind_usable THEN wind_direction END AS wind_direction,
        CASE WHEN precip_usable AND metar_type IS DISTINCT FROM 'SPECI' THEN precip_in END AS precip_in
    FROM usable
"#;

/// Every observation column a query reads, typed, so files written before a
/// column existed still load. Queries union it ahead of `read_parquet`.
const OBSERVATION_SOURCE_COLUMNS: &str = "
    SELECT NULL::VARCHAR AS station_id, NULL::VARCHAR AS generated_at,
           NULL::DOUBLE AS temperature_value, NULL::BIGINT AS wind_speed,
           NULL::BIGINT AS wind_direction,
           NULL::DOUBLE AS dewpoint_value, NULL::DOUBLE AS precip_in,
           NULL::VARCHAR AS temperature_unit_code,
           NULL::VARCHAR AS wx_string, NULL::VARCHAR AS filename,
           NULL::VARCHAR AS dewpoint_unit_code,
           NULL::VARCHAR AS quality_status, NULL::VARCHAR AS quality_reason,
           NULL::VARCHAR AS validation_version, NULL::VARCHAR AS raw_text,
           NULL::VARCHAR AS wind_speed_unit_code, NULL::VARCHAR AS wind_direction_unit_code,
           NULL::VARCHAR AS precip_unit_code, NULL::VARCHAR AS metar_type,
           NULL::VARCHAR AS quality_metrics
    WHERE false";

/// A report or forecast issue can reach a snapshot after its validity window.
const PUBLICATION_GRACE: Duration = Duration::hours(24);
const FORECAST_LOOKBACK: Duration = Duration::days(7);
/// How far from a day's part of a window a forecast period may lie for the
/// day's row to borrow from it (see [`forecasts_sql`]). Highs and lows each
/// come once a day in periods of 12 or 13 hours, so one is always this near.
const BORROW_REACH: Duration = Duration::hours(12);

/// Observation history, back from the newest file, the station list is read from.
const STATION_LOOKBACK: Duration = Duration::days(30);

/// Forecast files published this recently get derived copies: the widest
/// window a page or the coordinator reads (seven days of issues before a
/// week of comparisons) with a day to spare.
pub(crate) const DERIVED_WINDOW: Duration = Duration::days(10);

/// Queries running at once; more wait for a slot.
const MAX_CONCURRENT_QUERIES: usize = 4;
/// Limits of the database queries share. Together with a background copy
/// or fold (below) and the process's own caches this keeps the oracle near
/// 4 GB at its peak; queries that need more spill to disk.
const QUERIES_MEMORY_LIMIT: &str = "1536MB";
const QUERIES_THREADS: usize = 4;
/// How long queries share one database before a new one replaces it.
const DATABASE_LIFETIME: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
/// Limits of each background copy or fold, in its own database. Copying one
/// forecast file sorts all of its rows, source layouts included: a
/// 415,000-row, 87 MB file (September 2026) needs about 1 GB.
const QUERY_MEMORY_LIMIT: &str = "1536MB";
const QUERY_THREADS: usize = 2;
/// Limits of the database an eligibility history longer than the days read
/// ahead is read in (see [`WeatherAccess::query_alone`]). Thirty days of
/// reports through the window functions that deduplicate and screen them
/// filled the pool queries share and gigabytes beside it (October 2026).
/// Alone, in a smaller pool with fewer threads, the read spills to disk
/// instead, and what it held goes when it ends.
const LONG_READ_MEMORY_LIMIT: &str = "1GB";
const LONG_READ_THREADS: usize = 2;

/// DuckDB otherwise keeps the bytes of every Parquet file it reads in its
/// buffer pool until the memory limit is reached. The files are local and
/// the kernel's page cache holds them already, so that copy only fills the
/// pool and the process's resident memory with a second one.
const FILE_CACHE_OFF: &str = "SET enable_external_file_cache = false;";

/// Where DuckDB writes what a query cannot hold within its memory limit.
/// Its default, `.tmp` in the working directory, is read-only where the
/// oracle runs, so a query that outgrew its limit failed instead ("Failed to
/// create directory .tmp"). Set once, by [`WeatherAccess::with_derived_forecasts`].
static SPILL_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();

/// Spill directories of earlier processes untouched this long are left
/// over from a crash; a running query's spill files are fresh.
const STALE_SPILL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Removes other processes' spill directories under `spills` that have not
/// changed for [`STALE_SPILL`], keeping `own`.
fn remove_stale_spills(spills: &Path, own: &Path) {
    let Ok(entries) = std::fs::read_dir(spills) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_SPILL);
        if path != own && stale {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => log::info!("removed stale query spill directory {}", path.display()),
                Err(error) => log::warn!("cannot remove {}: {error}", path.display()),
            }
        }
    }
}

/// The `SET temp_directory` statement for [`SPILL_DIRECTORY`], if set.
fn spill_setting() -> String {
    SPILL_DIRECTORY
        .get()
        .map(|directory| {
            format!(
                " SET temp_directory = '{}';",
                directory.to_string_lossy().replace('\'', "''")
            )
        })
        .unwrap_or_default()
}

#[derive(Clone)]
pub struct WeatherAccess {
    file_access: Arc<dyn FileData>,
    slots: Arc<Semaphore>,
    /// One in-memory database for every query, and when it was opened, so
    /// queries share its memory limit and its cache of Parquet file
    /// metadata. Published files, copies and folds never change once
    /// written, so cached metadata never goes stale; the database is
    /// reopened daily so the cache drops files deleted since.
    database: Arc<Mutex<Option<(Instant, Connection)>>>,
    /// Reports eligibility is judged from, read ahead of requests.
    timeline: Arc<Mutex<Option<Arc<eligibility::Timeline>>>>,
    derived: Option<DerivedForecasts>,
    folds: Option<Folds>,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("observation coverage is incomplete for {stations:?}: {reason}")]
    ObservationCoverage {
        stations: Vec<String>,
        reason: String,
    },
    #[error("forecast inputs require review: {reason}")]
    ForecastQuality { reason: String },
    #[error("observation quality verification is unavailable")]
    QualityUnavailable,
    #[error(
        "observation quality verification is unavailable: {legacy_reports} reports come from files that predate the daemon's quality fields"
    )]
    LegacyObservations { legacy_reports: u64 },
    #[error(
        "observations require review: {rejected_reports} rejected reports, {unverified_reports} unverified reports"
    )]
    DataQuality {
        rejected_reports: u64,
        unverified_reports: u64,
    },
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

/// A metric over the exact requested settlement window. Missing values remain explicit.
#[derive(Clone, Debug, PartialEq)]
pub struct SettlementValue {
    pub station_id: String,
    pub metric: String,
    pub value: Option<f64>,
}

/// Valid native forecast boundaries from the selected source publication.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ForecastNativeInterval {
    pub metric: String,
    pub start: String,
    /// Absent for a forecast instant, present for an accumulation or extremum.
    pub end: Option<String>,
}

/// The same per-metric assessment used by settlement, with planning evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ForecastAssessment {
    pub station_id: String,
    pub metric: String,
    pub value: Option<f64>,
    pub reason: Option<String>,
    pub native_intervals: Vec<ForecastNativeInterval>,
}

#[async_trait]
pub trait WeatherData: Sync + Send {
    fn eligibility_cache_bytes(&self) -> usize {
        0
    }

    /// Discovery screening; final source verification remains part of event creation.
    async fn forecast_candidates(
        &self,
        _request: &ForecastRequest,
        station_ids: Vec<String>,
        _metrics: &[String],
    ) -> Result<Vec<String>, Error> {
        Ok(station_ids)
    }

    async fn forecasts_data(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<Forecast>, Error>;
    async fn settlement_forecasts(
        &self,
        _req: &ForecastRequest,
        _station_ids: Vec<String>,
    ) -> Result<Vec<SettlementValue>, Error> {
        Err(Error::QualityUnavailable)
    }
    async fn forecast_assessment(
        &self,
        _req: &ForecastRequest,
        _station_ids: Vec<String>,
    ) -> Result<Vec<ForecastAssessment>, Error> {
        Err(Error::QualityUnavailable)
    }
    /// The assessment [`WeatherData::forecast_assessment`] gives, for the
    /// check an event passes before it is created. Implementations that
    /// cannot assess return nothing, and creation is not checked.
    async fn planned_baselines(
        &self,
        _req: &ForecastRequest,
        _station_ids: Vec<String>,
    ) -> Result<Vec<ForecastAssessment>, Error> {
        Ok(vec![])
    }
    async fn calendar_forecasts_with_quality(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
        calendar: Calendar,
    ) -> Result<(Vec<Forecast>, ForecastQuality), Error> {
        Ok((
            self.calendar_forecasts(req, station_ids, calendar).await?,
            ForecastQuality::unavailable(),
        ))
    }
    /// Stations with recent verified fixed-hour precipitation reports.
    /// An empty list means capability is unknown, not unsupported.
    async fn precipitation_station_capabilities(
        &self,
        _station_ids: Vec<String>,
    ) -> Result<Vec<String>, Error> {
        Ok(vec![])
    }
    async fn observation_data(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<Observation>, Error>;
    /// Settlement must validate and aggregate the same selected publications.
    /// Implementations without this guarantee cannot authorize an attestation.
    ///
    /// Only report problems that affect `metrics`, the metrics the caller
    /// scores, hold settlement; no metrics means every metric.
    async fn settlement_observations(
        &self,
        _req: &ObservationRequest,
        _station_ids: Vec<String>,
        _required_collected_after: OffsetDateTime,
        _metrics: &[String],
    ) -> Result<Vec<SettlementObservation>, Error> {
        Err(Error::QualityUnavailable)
    }
    /// Counts reports whose problems affect `metrics` (every metric when empty).
    async fn observation_quality(
        &self,
        _req: &ObservationRequest,
        _station_ids: Vec<String>,
        _metrics: &[String],
    ) -> Result<ObservationQuality, Error> {
        Err(Error::QualityUnavailable)
    }
    /// Get daily aggregated observations (grouped by UTC date)
    async fn daily_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<DailyObservation>, Error>;
    async fn stations(&self) -> Result<Vec<Station>, Error>;
    /// Every station that reported in the last `days` full UTC days, and
    /// whether a competition of `window_hours` starting at `now` can be
    /// drawn from it, ordered by station id. Implementations that cannot
    /// read the reports settlement reads cannot tell.
    async fn eligible_stations(
        &self,
        _days: u32,
        _window_hours: u32,
        _now: OffsetDateTime,
    ) -> Result<Vec<Eligibility>, Error> {
        Err(Error::QualityUnavailable)
    }

    /// Reads ahead what [`WeatherData::eligible_stations`] judges, so
    /// histories of up to [`PRECOMPUTED_DAYS`] days are judged without
    /// reading files until the next call. Returns how many reports it
    /// holds. Implementations that read nothing ahead return 0.
    async fn read_ahead_eligibility(&self, _now: OffsetDateTime) -> Result<usize, Error> {
        Ok(0)
    }

    /// [`WeatherData::forecasts_data`] with each `date` a day of
    /// `calendar`, for a reader's local days. Implementations without
    /// calendars group by UTC day.
    async fn calendar_forecasts(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
        calendar: Calendar,
    ) -> Result<Vec<Forecast>, Error> {
        let _ = calendar;
        self.forecasts_data(req, station_ids).await
    }

    /// [`WeatherData::daily_observations`] with each `date` a day of
    /// `calendar`. Implementations without calendars group by UTC day.
    async fn calendar_daily_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
        calendar: Calendar,
    ) -> Result<Vec<DailyObservation>, Error> {
        let _ = calendar;
        self.daily_observations(req, station_ids).await
    }

    /// Makes query-ready copies of recent forecast files that lack one and
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
           NULL::VARCHAR AS generated_at, NULL::VARCHAR AS filename,
           NULL::VARCHAR AS forecast_interval_version, NULL::VARCHAR AS interval_kind, NULL::VARCHAR AS source_url, NULL::VARCHAR AS source_received_at, NULL::VARCHAR AS source_xml_sha256, NULL::VARCHAR AS source_location, NULL::VARCHAR AS source_layouts, NULL::VARCHAR AS quality_status, NULL::VARCHAR AS quality_reason
    WHERE false";

/// The order a query picks each station and period's row by: the newest
/// issue, then the newest publication of it, then the largest values, so
/// ties are deterministic. It is one total order, so the pick can be taken
/// in stages (see [`folds`]).
const DEDUPE_ORDER: &str = "generated_ts DESC NULLS LAST, published_ts DESC NULLS LAST, \
     source DESC NULLS LAST, max_temp DESC NULLS LAST, min_temp DESC NULLS LAST, \
     wind_speed DESC NULLS LAST, wind_direction DESC NULLS LAST, \
     relative_humidity_max DESC NULLS LAST, relative_humidity_min DESC NULLS LAST, \
     precip_chance DESC NULLS LAST, liquid_precipitation_amt DESC NULLS LAST, \
     snow_amt DESC NULLS LAST, snow_ratio DESC NULLS LAST, ice_amt DESC NULLS LAST";

/// The columns of a forecast row as queries read it, in order.
const FORECAST_ROW_COLUMNS: &str = "station_id, begin_ts, end_ts, generated_ts, published_ts, source, \
     min_temp, max_temp, wind_speed, wind_direction, relative_humidity_max, relative_humidity_min, \
     precip_chance, liquid_precipitation_amt, snow_amt, snow_ratio, ice_amt, forecast_interval_version, interval_kind, source_url, source_received_at, source_xml_sha256, source_location, source_layouts, quality_status, quality_reason";

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
            ice_amt,
            forecast_interval_version, interval_kind, source_url, source_received_at, source_xml_sha256, source_location, source_layouts, quality_status, quality_reason
        FROM ({FORECAST_SOURCE_COLUMNS}
              UNION ALL BY NAME
              SELECT * FROM read_parquet([{}], union_by_name = true, filename = true))
        {filter}"#,
        sql_string_list(files)
    )
}

/// A calendar day a forecast window covers only in part, with that part.
struct PartialDay {
    /// As queries name days: `YYYY-MM-DD 00:00:00`.
    date: String,
    start: OffsetDateTime,
    end: OffsetDateTime,
}

/// The days of `calendar` a request's window covers only in part: its first
/// day, its last day, both or neither.
fn partial_days(req: &ForecastRequest, calendar: Calendar) -> Vec<PartialDay> {
    let (Some(start), Some(end)) = (req.start, req.end) else {
        return vec![];
    };
    if start >= end {
        return vec![];
    }
    let first = calendar.date_of(start);
    let last = calendar.date_of(end - Duration::nanoseconds(1));
    let mut days = vec![];
    for day in [Some(first), (last != first).then_some(last)]
        .into_iter()
        .flatten()
    {
        let begins = calendar.start_of(day);
        let ends = calendar.start_of(day.next_day().unwrap_or(day));
        if start > begins || end < ends {
            days.push(PartialDay {
                date: format!(
                    "{:04}-{:02}-{:02} 00:00:00",
                    day.year(),
                    u8::from(day.month()),
                    day.day()
                ),
                start: start.max(begins),
                end: end.min(ends),
            });
        }
    }
    days
}

/// Which periods lie in a request's validity window: those overlapping it,
/// and point samples from its start on.
fn window_conditions(req: &ForecastRequest) -> Result<Vec<String>, Error> {
    let mut conditions = Vec::new();
    if let Some(start) = &req.start {
        conditions.push(format!(
            "(end_ts > '{}'::TIMESTAMPTZ OR (interval_kind = 'instant' AND begin_ts >= '{}'::TIMESTAMPTZ))",
            start.format(&Rfc3339)?, start.format(&Rfc3339)?
        ));
    }
    if let Some(end) = &req.end {
        conditions.push(format!(
            "begin_ts < '{}'::TIMESTAMPTZ",
            end.format(&Rfc3339)?
        ));
    }
    Ok(conditions)
}

/// Which forecast rows a request reads: periods in its validity window,
/// from issues generated in `generated_start..=generated_end`. For days the
/// window covers only in part (`partial`) it also reads the highs and lows
/// within [`BORROW_REACH`] of those parts, which their rows may borrow.
fn forecast_row_conditions(
    req: &ForecastRequest,
    (generated_start, generated_end): (OffsetDateTime, OffsetDateTime),
    partial: &[PartialDay],
) -> Result<String, Error> {
    let mut conditions = window_conditions(req)?;
    let reach = partial
        .iter()
        .map(|day| day.start)
        .min()
        .zip(partial.iter().map(|day| day.end).max());
    if let Some((from, to)) = reach {
        conditions = vec![format!(
            "(({}) OR ((min_temp IS NOT NULL OR max_temp IS NOT NULL) AND end_ts > '{}'::TIMESTAMPTZ AND begin_ts < '{}'::TIMESTAMPTZ))",
            conditions.join(" AND "),
            from.saturating_sub(BORROW_REACH).format(&Rfc3339)?,
            to.saturating_add(BORROW_REACH).format(&Rfc3339)?
        )];
    }
    conditions.push(format!(
        "generated_ts >= '{}'::TIMESTAMPTZ",
        generated_start.format(&Rfc3339)?
    ));
    conditions.push(format!(
        "generated_ts <= '{}'::TIMESTAMPTZ",
        generated_end.format(&Rfc3339)?
    ));
    Ok(conditions.join(" AND "))
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

/// Daily forecasts from `rows`, forecast rows (see [`FORECAST_ROW_COLUMNS`])
/// already restricted to the request: for each station and forecast period,
/// the newest issue, then the newest publication of it; then per day of
/// `calendar` the extremes, peak wind with its direction, humidity range,
/// precipitation chance, and precipitation totals. Rain is QPF minus the
/// liquid equivalent of snow and ice, never below zero.
///
/// A period counts toward the day it begins on. NOAA forecasts a high for
/// each daytime and a low for each night, so the part of a day a window
/// covers can hold one and not the other, or no wind sample. The row of
/// such a day (`partial`) borrows what it lacks from the nearest period
/// within [`BORROW_REACH`], one in the window first, so the days of a
/// window still combine to the window's own extremes. A borrowed extreme
/// never inverts the day's range.
fn forecasts_sql(
    rows: &str,
    req: &ForecastRequest,
    calendar: Calendar,
    (generated_start, generated_end): (OffsetDateTime, OffsetDateTime),
    partial: &[PartialDay],
) -> Result<String, Error> {
    let date = calendar.day_sql(
        "begin_ts",
        req.start.unwrap_or(generated_start),
        req.end.unwrap_or(generated_end + FORECAST_LOOKBACK),
    );
    let start_time = match &req.start {
        Some(start) => format!(
            "GREATEST('{}'::TIMESTAMPTZ, d.start_time, w.part_start)",
            start.format(&Rfc3339)?
        ),
        None => "d.start_time".to_owned(),
    };
    let end_time = match &req.end {
        Some(end) => format!(
            "LEAST('{}'::TIMESTAMPTZ, d.end_time, w.part_end)",
            end.format(&Rfc3339)?
        ),
        None => "d.end_time".to_owned(),
    };
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
    let native_period = format!(
        "STRUCT_PACK(\"start\" := {}, \"end\" := {}, version := forecast_interval_version, layouts := source_layouts)",
        utc_timestamp_sql("begin_ts"),
        utc_timestamp_sql("end_ts")
    );
    let in_window = match window_conditions(req)? {
        conditions if conditions.is_empty() => "true".to_owned(),
        conditions => conditions.join(" AND "),
    };
    // Each day's part of the window, and the span periods may be borrowed from.
    let partial_days = if partial.is_empty() {
        "SELECT NULL::VARCHAR AS date, NULL::TIMESTAMPTZ AS part_start, NULL::TIMESTAMPTZ AS part_end, \
         NULL::TIMESTAMPTZ AS near_start, NULL::TIMESTAMPTZ AS near_end WHERE false"
            .to_owned()
    } else {
        let mut days = vec![];
        for day in partial {
            days.push(format!(
                "('{}', '{}'::TIMESTAMPTZ, '{}'::TIMESTAMPTZ, '{}'::TIMESTAMPTZ, '{}'::TIMESTAMPTZ)",
                day.date,
                day.start.format(&Rfc3339)?,
                day.end.format(&Rfc3339)?,
                day.start.saturating_sub(BORROW_REACH).format(&Rfc3339)?,
                day.end.saturating_add(BORROW_REACH).format(&Rfc3339)?
            ));
        }
        format!(
            "SELECT * FROM (VALUES {}) AS days(date, part_start, part_end, near_start, near_end)",
            days.join(", ")
        )
    };
    // Nearest to the day's part of the window; of two equally near, the later.
    let nearest = "p.in_window DESC, \
         GREATEST(epoch(p.begin_ts) - epoch(w.part_end), epoch(w.part_start) - epoch(p.end_ts), 0), \
         p.begin_ts DESC, p.end_ts DESC";
    Ok(format!(
        r#"
    WITH forecast_rows AS (
        {rows}
    ),
    -- Per station and period, the newest issue, then the newest
    -- publication of that issue. Value ties are deterministic.
    selected AS (
        SELECT station_id, begin_ts, end_ts, in_window, picked.*,
            {date} AS date,
            EXTRACT(EPOCH FROM (end_ts - begin_ts)) AS duration_secs
        FROM (
            SELECT station_id, begin_ts, end_ts, ({in_window}) AS in_window,
                FIRST(STRUCT_PACK(
                    min_temp := min_temp, max_temp := max_temp,
                    wind_speed := wind_speed, wind_direction := wind_direction,
                    relative_humidity_max := relative_humidity_max,
                    relative_humidity_min := relative_humidity_min,
                    precip_chance := precip_chance,
                    liquid_precipitation_amt := liquid_precipitation_amt,
                    snow_amt := snow_amt, snow_ratio := snow_ratio, ice_amt := ice_amt,
                    quality_status := quality_status,
                    forecast_interval_version := forecast_interval_version,
                    source_layouts := source_layouts
                ) ORDER BY {DEDUPE_ORDER}) AS picked
            FROM forecast_rows
            GROUP BY station_id, begin_ts, end_ts, ({in_window})
        )
    ),
    -- Keep the newest publication even when quarantined; removing it before
    -- selection would silently restore an older value. Historical files have
    -- no producer status and remain provisional display data.
    periods AS (
        SELECT station_id, begin_ts, end_ts, in_window, date, duration_secs,
            quality_status, forecast_interval_version, source_layouts,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN min_temp END AS min_temp,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN max_temp END AS max_temp,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN wind_speed END AS wind_speed,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN wind_direction END AS wind_direction,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN relative_humidity_max END AS relative_humidity_max,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN relative_humidity_min END AS relative_humidity_min,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN precip_chance END AS precip_chance,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN liquid_precipitation_amt END AS liquid_precipitation_amt,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN snow_amt END AS snow_amt,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN snow_ratio END AS snow_ratio,
            CASE WHEN quality_status IS DISTINCT FROM 'rejected' THEN ice_amt END AS ice_amt
        FROM selected
    ),
    -- Periods outside the window are read only to be borrowed from.
    deduped AS (
        SELECT * EXCLUDE (in_window) FROM periods WHERE in_window
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
            MAX(precip_chance) AS precip_chance,
            COUNT(*) FILTER (WHERE quality_status = 'rejected') AS rejected_rows,
            COUNT(*) FILTER (WHERE forecast_interval_version IS DISTINCT FROM 'ndfd-native-v1'
                OR quality_status IS NULL OR source_layouts IS NULL) AS unverified_rows,
            to_json(arg_min({native_period}, min_temp) FILTER (WHERE min_temp >= -200 AND min_temp <= 200))::VARCHAR AS min_temp_period,
            to_json(arg_max({native_period}, max_temp) FILTER (WHERE max_temp >= -200 AND max_temp <= 200))::VARCHAR AS max_temp_period,
            to_json(arg_min({native_period}, relative_humidity_min) FILTER (WHERE relative_humidity_min >= 0 AND relative_humidity_min <= 100))::VARCHAR AS humidity_min_period,
            to_json(arg_max({native_period}, relative_humidity_max) FILTER (WHERE relative_humidity_max >= 0 AND relative_humidity_max <= 100))::VARCHAR AS humidity_max_period
        FROM deduped
        GROUP BY station_id, date
    ),
    -- The days the window covers only in part.
    partial_days AS (
        {partial_days}
    ),
    -- What the row of such a day lacks, from the nearest period that has it.
    -- Wind is borrowed only from samples in the window.
    borrowed AS (
        SELECT d.station_id, d.date,
            FIRST(p.min_temp ORDER BY {nearest})
                FILTER (WHERE p.min_temp >= -200 AND p.min_temp <= 200) AS temp_low,
            FIRST(p.max_temp ORDER BY {nearest})
                FILTER (WHERE p.max_temp >= -200 AND p.max_temp <= 200) AS temp_high,
            FIRST(p.wind_speed ORDER BY {nearest})
                FILTER (WHERE p.in_window AND p.wind_speed >= 0 AND p.wind_speed <= 500) AS wind_speed,
            FIRST(p.wind_direction ORDER BY {nearest})
                FILTER (WHERE p.in_window AND p.wind_speed >= 0 AND p.wind_speed <= 500) AS wind_direction
        FROM daily d
        JOIN partial_days w ON d.date = w.date
        JOIN periods p ON p.station_id = d.station_id
            AND p.end_ts > w.near_start AND p.begin_ts < w.near_end
        WHERE d.temp_low IS NULL OR d.temp_high IS NULL OR d.wind_speed IS NULL
        GROUP BY d.station_id, d.date
    )
    SELECT
        d.station_id::VARCHAR AS station_id,
        d.date::VARCHAR AS date,
        ({})::VARCHAR AS start_time,
        ({})::VARCHAR AS end_time,
        CASE WHEN d.temp_low > d.temp_high THEN NULL
            WHEN d.temp_low IS NOT NULL THEN d.temp_low
            WHEN b.temp_low > d.temp_high THEN d.temp_high
            ELSE b.temp_low END::BIGINT AS temp_low,
        CASE WHEN d.temp_low > d.temp_high THEN NULL
            WHEN d.temp_high IS NOT NULL THEN d.temp_high
            WHEN b.temp_high < COALESCE(d.temp_low, b.temp_low) THEN COALESCE(d.temp_low, b.temp_low)
            ELSE b.temp_high END::BIGINT AS temp_high,
        COALESCE(d.wind_speed, b.wind_speed)::BIGINT AS wind_speed,
        CASE WHEN d.wind_speed IS NULL THEN b.wind_direction ELSE d.wind_direction END::BIGINT AS wind_direction,
        CASE WHEN d.humidity_min > d.humidity_max THEN NULL ELSE d.humidity_max END::BIGINT AS humidity_max,
        CASE WHEN d.humidity_min > d.humidity_max THEN NULL ELSE d.humidity_min END::BIGINT AS humidity_min,
        'fahrenheit'::VARCHAR AS temperature_unit_code,
        d.precip_chance::DOUBLE AS precip_chance,
        CASE WHEN q.total_qpf IS NULL THEN NULL ELSE GREATEST(0,
            q.total_qpf - COALESCE(s.snow_liquid_amt, 0) - COALESCE(i.ice_amt, 0)
        ) END::DOUBLE AS rain_amt,
        s.snow_amt::DOUBLE AS snow_amt,
        i.ice_amt::DOUBLE AS ice_amt,
        d.rejected_rows::BIGINT AS rejected_rows, d.unverified_rows::BIGINT AS unverified_rows,
        d.temp_low::DOUBLE AS raw_temp_low, d.temp_high::DOUBLE AS raw_temp_high,
        d.humidity_min::DOUBLE AS raw_humidity_min, d.humidity_max::DOUBLE AS raw_humidity_max,
        d.min_temp_period, d.max_temp_period, d.humidity_min_period, d.humidity_max_period
    FROM daily d
    LEFT JOIN qpf_daily q ON d.station_id = q.station_id AND d.date = q.date
    LEFT JOIN snow_daily s ON d.station_id = s.station_id AND d.date = s.date
    LEFT JOIN ice_daily i ON d.station_id = i.station_id AND d.date = i.date
    LEFT JOIN borrowed b ON d.station_id = b.station_id AND d.date = b.date
    LEFT JOIN partial_days w ON d.date = w.date
    ORDER BY d.station_id, d.date
    "#,
        utc_timestamp_sql(&start_time),
        utc_timestamp_sql(&end_time),
    ))
}

/// Forecast files published in a request's issue window, or up to
/// [`PUBLICATION_GRACE`] after it, and never in the future.
fn forecast_file_params(
    (generated_start, generated_end): (OffsetDateTime, OffsetDateTime),
    now: OffsetDateTime,
) -> FileParams {
    FileParams {
        start: Some(generated_start.to_offset(UtcOffset::UTC)),
        end: Some(
            generated_end
                .saturating_add(PUBLICATION_GRACE)
                .min(now)
                .to_offset(UtcOffset::UTC),
        ),
        observations: Some(false),
        forecasts: Some(true),
    }
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

/// The issue window a forecast request reads: as given, or the week of
/// issues before its end or `now`, whichever is earlier.
pub(crate) fn forecast_generated_window(
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
    /// Quality counts and settlement's report verification consider
    /// problems that affect `metrics` (every metric when empty).
    async fn observation_batches(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
        coverage: Option<coverage::Requirement>,
        metrics: &[String],
    ) -> Result<Vec<RecordBatch>, Error> {
        let station_filter = station_filter(&station_ids)?;
        let groups = format!(
            "[{}]",
            sql_string_list(
                &quality_groups(metrics)
                    .into_iter()
                    .map(String::from)
                    .collect::<Vec<_>>()
            )
        );
        let file_params = observation_file_params(req, OffsetDateTime::now_utc());
        if empty_publication_window(&file_params) {
            if let Some(coverage) = &coverage {
                return Err(coverage.unavailable("the requested publication window is empty"));
            }
            return Ok(vec![]);
        }
        let parquet_files = self.file_access.grab_file_names(file_params).await?;
        let file_paths = self.file_access.build_file_paths(parquet_files);
        if file_paths.is_empty() {
            if let Some(coverage) = &coverage {
                return Err(
                    coverage.unavailable("no source-history files cover the requested window")
                );
            }
            return Ok(vec![]);
        }

        // Build time filter clauses
        let mut time_filters = Vec::new();
        if let Some(start) = &req.start {
            time_filters.push(format!(
                "generated_at::TIMESTAMPTZ >= '{}'::TIMESTAMPTZ",
                start.saturating_sub(Duration::hours(2)).format(&Rfc3339)?
            ));
        }
        // A later routine METAR can report precipitation beginning/ending
        // before the event ended. Read it only for phase closure; the separate
        // point bounds below still exclude every post-window extremum.
        let read_end = coverage
            .as_ref()
            .map(|value| value.end + Duration::minutes(75))
            .or(req.end);
        if let Some(end) = &read_end {
            time_filters.push(format!(
                "generated_at::TIMESTAMPTZ <= '{}'::TIMESTAMPTZ",
                end.format(&Rfc3339)?
            ));
        }

        let mut result_bounds = Vec::new();
        if let Some(start) = req.start {
            result_bounds.push(format!(
                "generated_at::TIMESTAMPTZ >= '{}'::TIMESTAMPTZ",
                start.format(&Rfc3339)?
            ));
        }
        if let Some(end) = req.end {
            result_bounds.push(format!(
                "generated_at::TIMESTAMPTZ <= '{}'::TIMESTAMPTZ",
                end.format(&Rfc3339)?
            ));
        }
        let result_time_filter = if result_bounds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", result_bounds.join(" AND "))
        };
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
                    {OBSERVATION_SOURCE_COLUMNS}
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
                COUNT(*) FILTER (WHERE qc_rejected AND list_has_any(qc_groups, {groups}))::BIGINT AS rejected_reports,
                COUNT(*) FILTER (WHERE NOT qc_rejected AND qc_unverified
                    AND list_has_any(qc_groups, {groups}))::BIGINT AS unverified_reports,
                COUNT(*) FILTER (WHERE NOT qc_rejected AND quality_status IS NULL
                    AND raw_text IS NULL)::BIGINT AS legacy_reports,
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
            {result_time_filter}
            GROUP BY station_id
            "#,
            sql_string_list(&file_paths),
            station_filter,
            time_filter,
            utc_timestamp_sql(&start_time_expr),
            utc_timestamp_sql(&end_time_expr),
        );

        let precipitation_sql = coverage.as_ref().map(|_| {
            // Reuse the exact immutable file selection and normalization. This
            // branch retains lookback anchors and the accumulation endpoint.
            let prefix = query_sql
                .split("            SELECT\n                station_id::VARCHAR AS station_id,")
                .next()
                .unwrap();
            format!(
                "{prefix} SELECT station_id, generated_at, metar_type, raw_text, source_precip_in, \
                NOT ((qc_rejected OR qc_unverified) AND list_has_any(qc_groups, {groups})) AS verified, \
                wind_speed IS NOT NULL, wind_direction IS NOT NULL, \
                ROUND(100.0 * EXP((17.625 * dewpoint_value) / (243.04 + dewpoint_value)) / \
                    EXP((17.625 * temperature_value) / (243.04 + temperature_value)))::BIGINT, \
                temperature_value IS NOT NULL \
                FROM classified ORDER BY station_id, generated_at::TIMESTAMPTZ"
            )
        });
        self.query_with_connection(query_sql, move |connection, batches| {
            if let Some(coverage) = coverage {
                decode_quality(batches)?.settleable()?;
                coverage.verify(connection, &file_paths)?;
                return precipitation::apply(
                    connection,
                    &file_paths,
                    precipitation_sql.as_deref().unwrap(),
                    &coverage,
                    batches,
                );
            }
            Ok(batches.to_vec())
        })
        .await
    }

    /// Reads published files only.
    pub fn new(file_access: Arc<dyn FileData>) -> Self {
        Self {
            file_access,
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_QUERIES - 1)),
            database: Arc::default(),
            timeline: Arc::default(),
            derived: None,
            folds: None,
        }
    }

    /// The ETL owns one query slot that public traffic cannot consume. Both
    /// handles share the database, its memory limit, and immutable file cache.
    pub fn settlement_access(&self) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(1)),
            ..self.clone()
        }
    }

    /// Also keeps query-ready copies of recent forecast files and folds of
    /// them in `directory` (see [`DerivedForecasts`] and [`Folds`]) and
    /// reads them instead.
    ///
    /// Queries that outgrow their memory limit spill to a directory of this
    /// process's own under `directory`, which copy and fold pruning leave
    /// alone.
    pub fn with_derived_forecasts(file_access: Arc<dyn FileData>, directory: &Path) -> Self {
        let spills = directory.join("duckdb-spill");
        let spill = spills.join(std::process::id().to_string());
        remove_stale_spills(&spills, &spill);
        match std::fs::create_dir_all(&spill) {
            Ok(()) => {
                let _ = SPILL_DIRECTORY.set(spill);
            }
            Err(error) => log::warn!(
                "cannot create the query spill directory {}: {error}",
                spill.display()
            ),
        }
        Self {
            derived: Some(DerivedForecasts::new(directory)),
            folds: Some(Folds::new(directory)),
            ..Self::new(file_access)
        }
    }

    /// The rows of the forecast files `names` issued in `issued` that a
    /// request reads, from folds of whole days, file copies or the
    /// published files, whichever exist.
    fn forecast_rows_sql(
        &self,
        names: Vec<String>,
        issued: (OffsetDateTime, OffsetDateTime),
        stations: Option<&str>,
        conditions: &str,
    ) -> String {
        let plan = match &self.folds {
            Some(folds) => folds.plan(names, issued),
            None => folds::Plan {
                folds: vec![],
                files: names,
            },
        };
        // Folds and copies have the same columns.
        let mut copies = plan.folds;
        let mut published = vec![];
        for name in plan.files {
            let Ok(file) = ParquetFileName::parse(&name) else {
                continue;
            };
            match self
                .derived
                .as_ref()
                .and_then(|derived| derived.existing(&file))
            {
                Some(copy) => copies.push(copy),
                None => published.push(self.file_access.build_file_path(&file)),
            }
        }
        let mut branches = vec![];
        if !copies.is_empty() {
            let stations = stations
                .map(|condition| format!("{condition} AND "))
                .unwrap_or_default();
            branches.push(format!(
                "SELECT {FORECAST_ROW_COLUMNS} FROM read_parquet([{}]) WHERE {stations}{conditions}",
                sql_string_list(&copies)
            ));
        }
        if !published.is_empty() {
            let stations = stations
                .map(|condition| format!("WHERE {condition}"))
                .unwrap_or_default();
            branches.push(format!(
                "SELECT {FORECAST_ROW_COLUMNS} FROM ({}) WHERE {conditions}",
                source_forecast_rows_sql(&published, &stations)
            ));
        }
        branches.join("\n        UNION ALL\n        ")
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

    /// Runs `sql` on a new connection to the shared database off the async
    /// runtime, and decodes the result there too. The query keeps its slot
    /// until it finishes, even if its caller is cancelled.
    async fn query<T: Send + 'static>(
        &self,
        sql: String,
        decode: impl FnOnce(&[RecordBatch]) -> Result<Vec<T>, Error> + Send + 'static,
    ) -> Result<Vec<T>, Error> {
        self.query_with_connection(sql, move |_, batches| decode(batches))
            .await
    }

    async fn query_with_connection<T: Send + 'static>(
        &self,
        sql: String,
        decode: impl FnOnce(&Connection, &[RecordBatch]) -> Result<Vec<T>, Error> + Send + 'static,
    ) -> Result<Vec<T>, Error> {
        let slot = self.slot().await?;
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let connection = clone_query_connection(&database)?;
            let mut statement = connection.prepare(&sql)?;
            let batches: Vec<RecordBatch> = statement.query_arrow([])?.collect();
            decode(&connection, &batches)
        })
        .await?
    }

    /// Runs `sql` as [`Self::query_with_connection`] does, but in a
    /// database of its own with [`LONG_READ_MEMORY_LIMIT`] and
    /// [`LONG_READ_THREADS`], closed when it ends: for reads that would
    /// otherwise crowd every other query out of the shared pool.
    async fn query_alone<T: Send + 'static>(
        &self,
        sql: String,
        decode: impl FnOnce(&Connection, &[RecordBatch]) -> Result<Vec<T>, Error> + Send + 'static,
    ) -> Result<Vec<T>, Error> {
        let slot = self.slot().await?;
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let connection = open_long_read_connection()?;
            let mut statement = connection.prepare(&sql)?;
            let batches: Vec<RecordBatch> = statement.query_arrow([])?.collect();
            decode(&connection, &batches)
        })
        .await?
    }
}

/// The in-memory database queries share.
fn clone_query_connection(
    database: &Mutex<Option<(Instant, Connection)>>,
) -> Result<Connection, Error> {
    let mut database = database.lock().unwrap_or_else(PoisonError::into_inner);
    let (opened, root) = match database.take() {
        Some((opened, root)) if opened.elapsed() < DATABASE_LIFETIME => (opened, root),
        // Queries still running keep the old database open.
        _ => (Instant::now(), open_database()?),
    };
    let connection = root.try_clone()?;
    *database = Some((opened, root));
    Ok(connection)
}

fn open_database() -> Result<Connection, duckdb::Error> {
    let connection = Connection::open_in_memory()?;
    connection.execute_batch(&format!(
        "SET memory_limit = '{QUERIES_MEMORY_LIMIT}'; SET threads = {QUERIES_THREADS};
         {FILE_CACHE_OFF}
         INSTALL parquet; LOAD parquet; SET parquet_metadata_cache = true;{}",
        spill_setting()
    ))?;
    Ok(connection)
}

/// A fresh in-memory database for one long read (see
/// [`LONG_READ_MEMORY_LIMIT`]).
fn open_long_read_connection() -> Result<Connection, duckdb::Error> {
    let connection = Connection::open_in_memory()?;
    connection.execute_batch(&format!(
        "SET memory_limit = '{LONG_READ_MEMORY_LIMIT}'; SET threads = {LONG_READ_THREADS};
         {FILE_CACHE_OFF}
         INSTALL parquet; LOAD parquet;{}",
        spill_setting()
    ))?;
    Ok(connection)
}

/// A fresh in-memory database with bounded memory and threads, for work
/// apart from queries.
fn open_connection() -> Result<Connection, duckdb::Error> {
    let connection = Connection::open_in_memory()?;
    connection.execute_batch(&format!(
        "SET memory_limit = '{QUERY_MEMORY_LIMIT}'; SET threads = {QUERY_THREADS};
         {FILE_CACHE_OFF}
         INSTALL parquet; LOAD parquet;{}",
        spill_setting()
    ))?;
    Ok(connection)
}
#[async_trait]
impl WeatherData for WeatherAccess {
    async fn forecast_candidates(
        &self,
        request: &ForecastRequest,
        station_ids: Vec<String>,
        metrics: &[String],
    ) -> Result<Vec<String>, Error> {
        self.available_forecasts(request, station_ids, metrics)
            .await
    }

    fn eligibility_cache_bytes(&self) -> usize {
        self.timeline
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map_or(0, |timeline| timeline.estimated_bytes())
    }

    async fn forecasts_data(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<Forecast>, Error> {
        self.calendar_forecasts(req, station_ids, Calendar::Utc)
            .await
    }

    async fn settlement_forecasts(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<SettlementValue>, Error> {
        Ok(self
            .native_forecast_assessment(req, station_ids)
            .await?
            .into_iter()
            .inspect(|assessment| {
                if let Some(reason) = &assessment.reason {
                    log::warn!(
                        "settlement forecast unavailable: station={} metric={} reason={reason}",
                        assessment.station_id,
                        assessment.metric
                    );
                }
            })
            .map(|assessment| SettlementValue {
                station_id: assessment.station_id,
                metric: assessment.metric,
                value: assessment.value,
            })
            .collect())
    }

    async fn precipitation_station_capabilities(
        &self,
        station_ids: Vec<String>,
    ) -> Result<Vec<String>, Error> {
        station_filter(&station_ids)?;
        let now = OffsetDateTime::now_utc();
        let req = ObservationRequest {
            start: Some(now - Duration::hours(3)),
            end: Some(now),
            station_ids: station_ids.join(","),
            temperature_unit: TemperatureUnit::Celsius,
        };
        let files = self
            .file_access
            .grab_file_names(observation_file_params(&req, now))
            .await?;
        let paths = self.file_access.build_file_paths(files);
        self.query_with_connection("SELECT 1".into(), move |connection, _| {
            Ok(shef::Evidence::read(connection, &paths)?.recent_stations(&station_ids, now))
        })
        .await
    }

    async fn forecast_assessment(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<ForecastAssessment>, Error> {
        self.native_forecast_assessment(req, station_ids).await
    }

    async fn planned_baselines(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<ForecastAssessment>, Error> {
        self.native_forecast_assessment(req, station_ids).await
    }

    async fn calendar_forecasts(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
        calendar: Calendar,
    ) -> Result<Vec<Forecast>, Error> {
        Ok(self
            .calendar_forecasts_with_quality(req, station_ids, calendar)
            .await?
            .0)
    }

    async fn calendar_forecasts_with_quality(
        &self,
        req: &ForecastRequest,
        station_ids: Vec<String>,
        calendar: Calendar,
    ) -> Result<(Vec<Forecast>, ForecastQuality), Error> {
        let stations = station_condition(&station_ids)?;
        let now = OffsetDateTime::now_utc();
        let window = forecast_generated_window(req, now);
        // Files are named by publication time, not forecast validity. A
        // qualifying issue can be published after the generation cutoff.
        let file_params = forecast_file_params(window, now);
        if empty_publication_window(&file_params) {
            return Ok((vec![], ForecastQuality::default()));
        }
        let names = self.file_access.grab_file_names(file_params).await?;
        if names.is_empty() {
            return Ok((vec![], ForecastQuality::default()));
        }
        let partial = partial_days(req, calendar);
        let rows = self.forecast_rows_sql(
            names,
            window,
            stations.as_deref(),
            &forecast_row_conditions(req, window, &partial)?,
        );
        if rows.is_empty() {
            return Ok((vec![], ForecastQuality::default()));
        }
        let query_sql = forecasts_sql(&rows, req, calendar, window, &partial)?;
        let unit = req.temperature_unit;
        let mut result = self
            .query(query_sql, move |batches| {
                Ok(vec![(
                    decode_forecasts(batches, &unit)?,
                    forecast_display_quality::decode(batches)?,
                )])
            })
            .await?;
        Ok(result.pop().unwrap_or_default())
    }

    async fn observation_data(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<Observation>, Error> {
        let batches = self
            .observation_batches(req, station_ids, None, &[])
            .await?;
        decode_observations(&batches, &req.temperature_unit)
    }

    async fn settlement_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
        required_collected_after: OffsetDateTime,
        metrics: &[String],
    ) -> Result<Vec<SettlementObservation>, Error> {
        let coverage = coverage::Requirement {
            start: req.start.ok_or(Error::QualityUnavailable)?,
            end: req.end.ok_or(Error::QualityUnavailable)?,
            stations: station_ids.clone(),
            collected_after: required_collected_after,
            groups: quality_groups(metrics),
            metrics: metrics.to_vec(),
        };
        let point_request = ObservationRequest {
            start: req.start,
            end: req.end.map(|end| end - Duration::nanoseconds(1)),
            station_ids: req.station_ids.clone(),
            temperature_unit: req.temperature_unit,
        };
        let batches = self
            .observation_batches(&point_request, station_ids, Some(coverage), metrics)
            .await?;
        decode_quality(&batches)?.settleable()?;
        decode_settlement_observations(&batches, &req.temperature_unit)
    }

    async fn observation_quality(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
        metrics: &[String],
    ) -> Result<ObservationQuality, Error> {
        decode_quality(
            &self
                .observation_batches(req, station_ids, None, metrics)
                .await?,
        )
    }

    async fn daily_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<DailyObservation>, Error> {
        self.calendar_daily_observations(req, station_ids, Calendar::Utc)
            .await
    }

    async fn calendar_daily_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
        calendar: Calendar,
    ) -> Result<Vec<DailyObservation>, Error> {
        let now = OffsetDateTime::now_utc();
        let date = calendar.day_sql(
            "generated_at::TIMESTAMPTZ",
            req.start.unwrap_or(now - FORECAST_LOOKBACK),
            req.end.unwrap_or(now),
        );
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
                start.saturating_sub(Duration::hours(2)).format(&Rfc3339)?
            ));
        }
        if let Some(end) = &req.end {
            time_filters.push(format!(
                "generated_at::TIMESTAMPTZ <= '{}'::TIMESTAMPTZ",
                end.format(&Rfc3339)?
            ));
        }

        let result_time_filter = req
            .start
            .map(|start| {
                start.format(&Rfc3339).map(|start| {
                    format!("WHERE generated_at::TIMESTAMPTZ >= '{start}'::TIMESTAMPTZ")
                })
            })
            .transpose()?
            .unwrap_or_default();
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
        let dedup = DEDUP_OBSERVATIONS_SQL;
        let normalized = NORMALIZE_OBSERVATIONS_SQL;
        let query_sql = format!(
            r#"
            WITH parquet_data AS (
                SELECT * FROM (
                    {OBSERVATION_SOURCE_COLUMNS}
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
                {date} AS date,
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
            {result_time_filter}
            GROUP BY station_id, {date}
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

    async fn eligible_stations(
        &self,
        days: u32,
        window_hours: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<Eligibility>, Error> {
        self.eligibility(days, window_hours, now).await
    }

    async fn read_ahead_eligibility(&self, now: OffsetDateTime) -> Result<usize, Error> {
        self.read_ahead_reports(now).await
    }

    async fn prepare_files(&self, stopping: &CancellationToken) -> Result<usize, Error> {
        let Some(derived) = &self.derived else {
            return Ok(0);
        };
        let now = OffsetDateTime::now_utc();
        let oldest = now - DERIVED_WINDOW;
        let all_files: Vec<ParquetFileName> = self
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
            .collect();
        let mut files: Vec<_> = all_files
            .iter()
            .filter(|file| derived.missing(file))
            .cloned()
            .collect();
        // Newest first: current pages read the newest files.
        files.sort_by_key(|file| std::cmp::Reverse(file.generated_at));
        let mut made = 0;
        for file in files {
            if stopping.is_cancelled() {
                break;
            }
            let slot = self.slot().await?;
            let source = self.file_access.build_file_path(&file);
            let derived = derived.clone();
            let copied = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                derived.copy(&file, &source)
            })
            .await??;
            if copied == derived::Copied::Made {
                made += 1;
            }
        }
        if let Some(folds) = &self.folds
            && !stopping.is_cancelled()
        {
            let copies: Vec<_> = all_files
                .iter()
                .filter_map(|file| derived.existing(file).map(|copy| (file.clone(), copy)))
                .collect();
            let slot = self.slot().await?;
            let folds = folds.clone();
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                folds.fold_all(&copies)
            })
            .await??;
        }
        if let Some(oldest) = oldest.to_offset(UtcOffset::UTC).date().previous_day() {
            let derived = derived.clone();
            let folds = self.folds.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                derived.prune(oldest)?;
                if let Some(folds) = folds {
                    folds.prune(oldest)?;
                }
                Ok(())
            })
            .await??;
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

/// Counts describe the selected latest publications, before removing unusable values.
#[derive(Default, Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ObservationQuality {
    pub rejected_reports: u64,
    pub unverified_reports: u64,
    /// Unverified reports from files that predate the daemon's quality
    /// fields: no check ever ran on them.
    #[serde(default)]
    pub legacy_reports: u64,
}

impl ObservationQuality {
    /// Whether settlement may use the reports, or why not. Unverified
    /// reports drop out of the metrics their problems affect, and coverage
    /// then decides whether enough remain; rejected and legacy reports hold
    /// settlement.
    fn settleable(&self) -> Result<(), Error> {
        if self.rejected_reports == 0 && self.legacy_reports == 0 {
            Ok(())
        } else if self.rejected_reports == 0 && self.legacy_reports == self.unverified_reports {
            Err(Error::LegacyObservations {
                legacy_reports: self.legacy_reports,
            })
        } else {
            Err(Error::DataQuality {
                rejected_reports: self.rejected_reports,
                unverified_reports: self.unverified_reports,
            })
        }
    }
}

/// Metric groups the daemon tags report problems with (`quality_metrics`).
/// `NORMALIZE_OBSERVATIONS_SQL` repeats this list.
const QUALITY_GROUPS: [&str; 5] = [
    "temperature",
    "dewpoint",
    "wind",
    "precipitation",
    "present_weather",
];

/// The groups whose problems can change `metrics`. No metrics, or a metric
/// not listed here, means every group, as before reports were tagged.
pub fn quality_groups(metrics: &[String]) -> Vec<&'static str> {
    let mut groups = Vec::new();
    for metric in metrics {
        let affected: &[&'static str] = match metric.as_str() {
            "temp_high" | "temp_low" => &["temperature"],
            "humidity" => &["temperature", "dewpoint"],
            "wind_speed" | "wind_direction" => &["wind"],
            // Phase comes from present weather, or from temperature without it.
            "rain_amt" | "snow_amt" => &["precipitation", "present_weather", "temperature"],
            _ => &QUALITY_GROUPS,
        };
        for group in affected {
            if !groups.contains(group) {
                groups.push(*group);
            }
        }
    }
    if groups.is_empty() {
        QUALITY_GROUPS.to_vec()
    } else {
        groups
    }
}

fn decode_quality(batches: &[RecordBatch]) -> Result<ObservationQuality, Error> {
    let mut quality = ObservationQuality::default();
    for batch in batches {
        let rejected = integers(batch, "rejected_reports")?;
        let unverified = integers(batch, "unverified_reports")?;
        let legacy = integers(batch, "legacy_reports")?;
        for row in 0..batch.num_rows() {
            quality.rejected_reports += integer(rejected, row).unwrap_or(0).max(0) as u64;
            quality.unverified_reports += integer(unverified, row).unwrap_or(0).max(0) as u64;
            quality.legacy_reports += integer(legacy, row).unwrap_or(0).max(0) as u64;
        }
    }
    Ok(quality)
}

/// Settlement decodes each metric independently. The public observation wire
/// shape keeps required temperatures; it must not decide wind availability.
fn decode_settlement_observations(
    batches: &[RecordBatch],
    unit: &TemperatureUnit,
) -> Result<Vec<SettlementObservation>, Error> {
    let mut observations = vec![];
    for batch in batches {
        let station_id = strings(batch, "station_id")?;
        let temp_low = doubles(batch, "temp_low")?;
        let temp_high = doubles(batch, "temp_high")?;
        let unit_code = strings(batch, "temperature_unit_code")?;
        let wind_speed = integers(batch, "wind_speed")?;
        let wind_direction = integers(batch, "wind_direction")?;
        let humidity = integers(batch, "humidity")?;
        let rain_amt = doubles(batch, "rain_amt")?;
        let snow_amt = doubles(batch, "snow_amt")?;
        for row in 0..batch.num_rows() {
            let Some(station_id) = station(station_id, row) else {
                skipped("settlement observation", row);
                continue;
            };
            let code = text(unit_code, row).unwrap_or_default();
            observations.push(SettlementObservation {
                station_id,
                temp_low: double(temp_low, row).map(|value| convert(value, &code, unit)),
                temp_high: double(temp_high, row).map(|value| convert(value, &code, unit)),
                wind_speed: within(integer(wind_speed, row), 0..=500),
                wind_direction: within(integer(wind_direction, row), 0..=360),
                humidity: within(integer(humidity, row), 0..=100),
                rain_amt: non_negative(double(rain_amt, row)),
                snow_amt: non_negative(double(snow_amt, row)),
            });
        }
    }
    Ok(observations)
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

#[derive(Clone, Serialize, Deserialize, Debug, ToSchema)]
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

/// Independently optional measurements for the metrics settlement can score.
/// Source validation decides availability before these values are decoded.
#[derive(Debug)]
pub struct SettlementObservation {
    pub station_id: String,
    pub temp_low: Option<f64>,
    pub temp_high: Option<f64>,
    pub wind_speed: Option<i64>,
    pub wind_direction: Option<i64>,
    pub humidity: Option<i64>,
    pub rain_amt: Option<f64>,
    pub snow_amt: Option<f64>,
}

impl From<Observation> for SettlementObservation {
    fn from(observation: Observation) -> Self {
        Self {
            station_id: observation.station_id,
            temp_low: Some(observation.temp_low),
            temp_high: Some(observation.temp_high),
            wind_speed: observation.wind_speed,
            wind_direction: observation.wind_direction,
            humidity: observation.humidity,
            rain_amt: observation.rain_amt,
            snow_amt: observation.snow_amt,
        }
    }
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

    /// Queries that outgrow their memory limit spill under the derived
    /// directory, not `.tmp` in a working directory that may be read-only.
    #[test]
    fn queries_spill_to_a_directory_of_their_own() {
        let directory = tempfile::tempdir().unwrap();
        let _ = WeatherAccess::with_derived_forecasts(
            Arc::new(crate::file_access::FileAccess::new(
                directory.path().to_string_lossy().into_owned(),
            )),
            &directory.path().join("derived"),
        );
        // Another test may have set the process's spill directory first,
        // in a temporary directory that is gone by now; DuckDB creates it
        // again when it spills.
        let spill = SPILL_DIRECTORY.get().expect("spill directory set");
        assert!(spill.ends_with(std::process::id().to_string()));
        assert_eq!(spill.parent().unwrap().file_name().unwrap(), "duckdb-spill");
        for connection in [open_connection().unwrap(), open_database().unwrap()] {
            let setting: String = connection
                .query_row("SELECT current_setting('temp_directory')", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(std::path::Path::new(&setting), spill.as_path());
        }
    }

    #[test]
    fn stale_spill_directories_of_other_processes_are_removed() {
        let spills = tempfile::tempdir().unwrap();
        let own = spills.path().join("100");
        let fresh = spills.path().join("200");
        let stale = spills.path().join("300");
        for directory in [&own, &fresh, &stale] {
            std::fs::create_dir_all(directory).unwrap();
            std::fs::write(directory.join("duckdb_temp_block"), b"x").unwrap();
        }
        let old = std::time::SystemTime::now() - STALE_SPILL - std::time::Duration::from_secs(60);
        for directory in [&own, &stale] {
            std::fs::File::open(directory)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        remove_stale_spills(spills.path(), &own);
        assert!(own.exists(), "this process's directory stays, however old");
        assert!(fresh.exists(), "another process's recent spills stay");
        assert!(!stale.exists());
    }

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

    // Quality fixtures must not inherit unrelated station reports from the
    // multi-station historical compatibility file.
    fn quality_data_dir(extra: &[(&str, &str)]) -> tempfile::TempDir {
        let directory = data_dir(extra);
        std::fs::remove_file(
            directory
                .path()
                .join("2026-01-17/observations_2026-01-17T17:16:19.76658783Z.parquet"),
        )
        .unwrap();
        directory
    }

    #[tokio::test]
    async fn rounded_half_degree_dewpoint_difference_has_float_tolerance() {
        let directory = quality_data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT 'K40B' AS station_id, '2026-01-17T18:51:00Z' AS generated_at,
                    -1.1::DOUBLE AS temperature_value, -0.6::DOUBLE AS dewpoint_value,
                    'celsius' AS temperature_unit_code, 'celsius' AS dewpoint_unit_code,
                    'validated' AS quality_status, 'metar-consistency-v1' AS validation_version,
                    'K40B 171851Z M01/M01' AS raw_text",
        )]);
        let request = ObservationRequest {
            station_ids: "K40B".into(),
            ..day_request()
        };
        assert_eq!(
            access(&directory)
                .observation_quality(&request, request.station_ids(), &[])
                .await
                .unwrap()
                .rejected_reports,
            0
        );
    }

    #[tokio::test]
    async fn validation_labels_cannot_hide_missing_temperature_or_wrong_metric_units() {
        let directory = quality_data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT station_id, '2026-01-17T18:51:00Z' AS generated_at,
                    temperature_value::DOUBLE AS temperature_value, 'celsius' AS temperature_unit_code,
                    10::BIGINT AS wind_speed, wind_speed_unit_code, 1.0::DOUBLE AS precip_in, precip_unit_code,
                    'validated' AS quality_status, 'metar-consistency-v1' AS validation_version,
                    'METAR 171851Z 20/10' AS raw_text
             FROM (VALUES
                 ('KMISS', NULL, 'knots', 'inches'),
                 ('KWIND', 20.0, 'meters_per_second', 'inches'),
                 ('KRAIN', 20.0, 'knots', 'millimeters')
             ) AS reports(station_id, temperature_value, wind_speed_unit_code, precip_unit_code)",
        )]);
        let request = ObservationRequest {
            station_ids: "KMISS,KWIND,KRAIN".into(),
            ..day_request()
        };
        let weather = access(&directory);
        assert_eq!(
            weather
                .observation_quality(&request, request.station_ids(), &[])
                .await
                .unwrap()
                .rejected_reports,
            3
        );
        assert!(matches!(
            weather
                .settlement_observations(&request, request.station_ids(), request.end.unwrap(), &[])
                .await,
            Err(Error::DataQuality {
                rejected_reports: 3,
                ..
            })
        ));
        assert!(
            weather
                .observation_data(&request, request.station_ids())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn raw_consistent_spike_is_flagged_using_reports_before_window_start() {
        let directory = quality_data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT 'KS24' AS station_id, generated_at,
                    temperature_value::DOUBLE AS temperature_value,
                    'celsius' AS temperature_unit_code,
                    'validated' AS quality_status, 'metar-consistency-v1' AS validation_version,
                    raw_text
             FROM (VALUES
                 ('2026-01-17T17:51:00Z', 19.0, 'KS24 171751Z 19/10'),
                 ('2026-01-17T18:51:00Z', 1.0, 'KS24 171851Z 01/01')
             ) AS reports(generated_at, temperature_value, raw_text)",
        )]);
        let request = ObservationRequest {
            station_ids: "KS24".into(),
            start: Some(time::macros::datetime!(2026-01-17 18:00 UTC)),
            ..day_request()
        };
        let weather = access(&directory);
        assert_eq!(
            weather
                .observation_quality(&request, request.station_ids(), &[])
                .await
                .unwrap()
                .rejected_reports,
            1
        );
        assert!(
            weather
                .observation_data(&request, request.station_ids())
                .await
                .unwrap()
                .is_empty(),
            "context report must not leak into the requested window"
        );
        assert!(matches!(
            weather
                .settlement_observations(&request, request.station_ids(), request.end.unwrap(), &[])
                .await,
            Err(Error::DataQuality {
                rejected_reports: 1,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn latest_observation_publication_orders_instants_before_filename_ties() {
        let reports = |rows: &str| {
            format!(
                "SELECT station_id, '2026-01-17T18:51:00Z' AS generated_at,
                        temperature_value::DOUBLE AS temperature_value, 'celsius' AS temperature_unit_code,
                        'validated' AS quality_status, 'metar-consistency-v1' AS validation_version,
                        'METAR ' || station_id || ' 171851Z 12/06' AS raw_text
                 FROM (VALUES {rows}) AS reports(station_id, temperature_value)"
            )
        };
        let directory = quality_data_dir(&[
            (
                "observations_2026-01-17T21:00:00Z.parquet",
                &reports("('KTIM', 11.0), ('KTIE', 11.0)"),
            ),
            (
                "observations_2026-01-17T22:00:00+01:00.parquet",
                &reports("('KTIM', 12.0), ('KTIE', 12.0)"),
            ),
            (
                "observations_2026-01-17T21:30:00Z.parquet",
                &reports("('KTIM', 13.0)"),
            ),
        ]);
        let request = ObservationRequest {
            station_ids: "KTIM,KTIE".into(),
            ..day_request()
        };
        let weather = access(&directory);
        let observations = weather
            .observation_data(&request, request.station_ids())
            .await
            .unwrap();
        let temperature = |id| {
            observations
                .iter()
                .find(|observation| observation.station_id == id)
                .unwrap()
                .temp_high
        };
        assert!((temperature("KTIM") - 55.4).abs() < 1e-6);
        assert!((temperature("KTIE") - 53.6).abs() < 1e-6);
        let quality = weather
            .observation_quality(&request, request.station_ids(), &[])
            .await
            .unwrap();
        assert_eq!(quality.rejected_reports, 0);
    }

    #[tokio::test]
    async fn same_publication_conflict_requires_review_even_when_values_are_plausible() {
        let directory = quality_data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT 'KPWM' AS station_id, '2026-01-17T18:51:00Z' AS generated_at,
                    temperature_value::DOUBLE AS temperature_value, 'celsius' AS temperature_unit_code,
                    'validated' AS quality_status, 'metar-consistency-v1' AS validation_version,
                    'KPWM 171851Z 19/06' AS raw_text
             FROM (VALUES (19.0), (18.0)) AS reports(temperature_value)",
        )]);
        let request = ObservationRequest {
            station_ids: "KPWM".into(),
            ..day_request()
        };
        assert!(matches!(
            access(&directory)
                .settlement_observations(&request, request.station_ids(), request.end.unwrap(), &[])
                .await,
            Err(Error::DataQuality {
                rejected_reports: 1,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn portland_extreme_is_visible_in_quality_and_blocked_from_settlement() {
        let directory = quality_data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT 'KPWM' AS station_id, generated_at,
                    temperature_value::DOUBLE AS temperature_value,
                    'celsius' AS temperature_unit_code,
                    quality_status, 'metar-consistency-v1' AS validation_version,
                    'METAR KPWM 171851Z 19/06 RMK T01890056' AS raw_text
             FROM (VALUES
                 ('2026-01-17T17:51:00Z', 18.3, 'validated'),
                 ('2026-01-17T18:51:00Z', 60.0, 'rejected')
             ) AS reports(generated_at, temperature_value, quality_status)",
        )]);
        let request = ObservationRequest {
            station_ids: "KPWM".into(),
            ..day_request()
        };
        let weather = access(&directory);
        let values = weather
            .observation_data(&request, request.station_ids())
            .await
            .unwrap();
        assert!((values[0].temp_high - 64.94).abs() < 1e-6);
        assert_eq!(
            values[0].latest_temp_time.as_deref(),
            Some("2026-01-17T17:51:00Z")
        );
        let daily = weather
            .daily_observations(&request, request.station_ids())
            .await
            .unwrap();
        assert!((daily[0].temp_high - 64.94).abs() < 1e-6);
        let quality = weather
            .observation_quality(&request, request.station_ids(), &[])
            .await
            .unwrap();
        assert_eq!(quality.rejected_reports, 1);
        assert_eq!(quality.unverified_reports, 0);
        assert!(matches!(
            weather
                .settlement_observations(&request, request.station_ids(), request.end.unwrap(), &[])
                .await,
            Err(Error::DataQuality {
                rejected_reports: 1,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn all_rejected_reports_do_not_become_a_clean_empty_window() {
        let directory = quality_data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT 'KPWM' AS station_id, '2026-01-17T18:51:00Z' AS generated_at,
                    60.0::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code",
        )]);
        let request = ObservationRequest {
            station_ids: "KPWM".into(),
            ..day_request()
        };
        let weather = access(&directory);
        assert!(
            weather
                .observation_data(&request, request.station_ids())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            weather
                .settlement_observations(&request, request.station_ids(), request.end.unwrap(), &[])
                .await,
            Err(Error::DataQuality {
                rejected_reports: 1,
                ..
            })
        ));
        assert_eq!(
            weather
                .observation_quality(&request, request.station_ids(), &[])
                .await
                .unwrap()
                .rejected_reports,
            1
        );
    }

    #[tokio::test]
    async fn plausible_legacy_reports_are_not_silently_verified() {
        let directory = quality_data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT 'KPWM' AS station_id, '2026-01-17T18:51:00Z' AS generated_at,
                    18.9::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code",
        )]);
        let request = ObservationRequest {
            station_ids: "KPWM".into(),
            ..day_request()
        };
        let weather = access(&directory);
        assert_eq!(
            weather
                .observation_data(&request, request.station_ids())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(matches!(
            weather
                .settlement_observations(&request, request.station_ids(), request.end.unwrap(), &[])
                .await,
            Err(Error::LegacyObservations { legacy_reports: 1 })
        ));
        let quality = weather
            .observation_quality(&request, request.station_ids(), &[])
            .await
            .unwrap();
        assert_eq!(
            (
                quality.rejected_reports,
                quality.unverified_reports,
                quality.legacy_reports
            ),
            (0, 1, 1)
        );
    }

    #[tokio::test]
    async fn legacy_reports_mixed_with_checked_problems_still_count_as_unverified() {
        let directory = quality_data_dir(&[
            (
                "observations_2026-01-17T19:00:00Z.parquet",
                "SELECT 'KPWM' AS station_id, '2026-01-17T17:51:00Z' AS generated_at,
                        18.9::DOUBLE AS temperature_value, 'celcius' AS temperature_unit_code",
            ),
            (
                "observations_2026-01-17T21:00:00Z.parquet",
                "SELECT 'KPWM' AS station_id, '2026-01-17T18:51:00Z' AS generated_at,
                        18.9::DOUBLE AS temperature_value, 'celsius' AS temperature_unit_code,
                        'unverified' AS quality_status, 'metar-consistency-v1' AS validation_version,
                        'KPWM 171851Z 40/30 RMK T01890056' AS raw_text,
                        'temperature,dewpoint' AS quality_metrics",
            ),
        ]);
        let request = ObservationRequest {
            station_ids: "KPWM".into(),
            ..day_request()
        };
        assert!(matches!(
            access(&directory)
                .settlement_observations(&request, request.station_ids(), request.end.unwrap(), &[])
                .await,
            Err(Error::DataQuality {
                rejected_reports: 0,
                unverified_reports: 2
            })
        ));
    }

    #[test]
    fn metrics_map_onto_the_groups_the_daemon_tags() {
        let groups = |metrics: &[&str]| {
            quality_groups(&metrics.iter().map(|m| m.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(groups(&["temp_high", "temp_low"]), ["temperature"]);
        assert_eq!(
            groups(&["temp_high", "wind_speed", "humidity"]),
            ["temperature", "wind", "dewpoint"]
        );
        assert_eq!(
            groups(&["rain_amt"]),
            ["precipitation", "present_weather", "temperature"]
        );
        assert_eq!(groups(&[]), QUALITY_GROUPS);
        assert_eq!(groups(&["temp_high", "a_new_metric"]).len(), 5);
        // The SQL fallback list must match the groups the oracle knows.
        let listed = format!(
            "[{}]",
            QUALITY_GROUPS.map(|group| format!("'{group}'")).join(", ")
        );
        assert_eq!(NORMALIZE_OBSERVATIONS_SQL.matches(&listed).count(), 2);
    }

    /// Hourly KORD reports with a collection receipt, so settlement can
    /// succeed. The 19:00 report carries the highest temperature and wind
    /// and is flagged with `status` and `tags`.
    fn tagged_settlement_dir(status: &str, tags: Option<&str>) -> tempfile::TempDir {
        tagged_settlement_dir_with(status, tags, "", "validated")
    }

    /// As [`tagged_settlement_dir`], with validated reports at 18:30 and
    /// 19:30 too, so the flagged report can drop out of a metric without
    /// leaving a gap over 90 minutes.
    fn half_hourly_settlement_dir(status: &str, tags: Option<&str>) -> tempfile::TempDir {
        tagged_settlement_dir_with(
            status,
            tags,
            ",
            ('2026-01-17T18:30:00Z',11.5,8,'validated','METAR KORD 171830Z 09008KT 10SM 12/10 RMK AO2'),
            ('2026-01-17T19:30:00Z',12.5,8,'validated','METAR KORD 171930Z 09008KT 10SM 13/10 RMK AO2')",
            "validated",
        )
    }

    fn tagged_settlement_dir_with(
        status: &str,
        tags: Option<&str>,
        more_reports: &str,
        routine_status: &str,
    ) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let day = directory.path().join("2026-01-17");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join("observations_2026-01-17T20:30:00Z.parquet");
        let receipt = serde_json::json!({
            "version":"awc-history-v1", "interval":"closed", "batches":[{
                "station_ids":["KORD"], "window_start":"2026-01-17T17:00:00Z",
                "window_end":"2026-01-17T20:29:59Z", "requested_at":"2026-01-17T20:30:00Z",
                "completed_at":"2026-01-17T20:31:00Z", "status":"complete",
                "source_url":"https://aviationweather.gov/api/data/metar?ids=KORD&format=xml",
                "source_sha256":"a".repeat(64),"response_status":200,"response_count":6,"report_count":6,"error":null
            }]
        })
        .to_string()
        .replace('\'', "''");
        let tags = tags.map_or("NULL".into(), |tags| format!("'{tags}'"));
        open_connection()
            .unwrap()
            .execute_batch(&format!(
                r#"
            COPY (
                SELECT 'KORD' AS station_id, generated_at, temperature_value::DOUBLE AS temperature_value,
                    'celsius' AS temperature_unit_code, 'METAR' AS metar_type, raw_text,
                    0.0::DOUBLE AS precip_in, 'inches' AS precip_unit_code, '' AS wx_string,
                    wind_speed::BIGINT AS wind_speed, 'knots' AS wind_speed_unit_code,
                    90::BIGINT AS wind_direction, 'degrees true' AS wind_direction_unit_code,
                    (temperature_value-2)::DOUBLE AS dewpoint_value, 'celsius' AS dewpoint_unit_code,
                    quality_status, 'metar-consistency-v1' AS validation_version,
                    CASE WHEN quality_status = 'validated' THEN NULL ELSE 'flagged' END AS quality_reason,
                    CASE WHEN quality_status = 'validated' THEN NULL ELSE {tags} END AS quality_metrics
                FROM (VALUES
                    ('2026-01-17T17:00:00Z',10,8,'{routine_status}','METAR KORD 171700Z 09008KT 10SM 10/08 RMK AO2'),
                    ('2026-01-17T18:00:00Z',11,8,'{routine_status}','METAR KORD 171800Z 09008KT 10SM 11/09 RMK AO2'),
                    ('2026-01-17T19:00:00Z',14,20,'{status}','METAR KORD 171900Z 09020KT 10SM 14/12 RMK AO2 PNO'),
                    ('2026-01-17T20:00:00Z',12,8,'{routine_status}','METAR KORD 172000Z 09008KT 10SM 12/10 RMK AO2')
                    {more_reports}
                ) AS reports(generated_at,temperature_value,wind_speed,quality_status,raw_text)
            ) TO '{}' (FORMAT PARQUET, KV_METADATA {{observation_coverage:'{receipt}'}})
        "#,
                path.display()
            ))
            .unwrap();
        directory
    }

    async fn settle(
        directory: &tempfile::TempDir,
        metrics: &[&str],
    ) -> Result<Vec<SettlementObservation>, Error> {
        let request = ObservationRequest {
            start: Some(time::macros::datetime!(2026-01-17 18:00 UTC)),
            end: Some(time::macros::datetime!(2026-01-17 20:00 UTC)),
            station_ids: "KORD".into(),
            temperature_unit: TemperatureUnit::Celsius,
        };
        let metrics: Vec<String> = metrics.iter().map(|m| m.to_string()).collect();
        access(directory)
            .settlement_observations(
                &request,
                vec!["KORD".into()],
                time::macros::datetime!(2026-01-17 20:15 UTC),
                &metrics,
            )
            .await
    }

    #[tokio::test]
    async fn precipitation_only_problems_do_not_hold_temperature_or_wind_events() {
        let directory = tagged_settlement_dir("rejected", Some("precipitation"));
        let rows = settle(&directory, &["temp_high", "temp_low", "wind_speed"])
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].temp_high,
            Some(14.0),
            "a rain gauge outage leaves the report's temperature usable"
        );
        assert_eq!(rows[0].wind_speed, Some(20));
        assert_eq!(rows[0].rain_amt, None, "unscored rain is not verified");
        for metrics in [&["rain_amt"][..], &["temp_high", "snow_amt"], &[]] {
            assert!(
                matches!(
                    settle(&directory, metrics).await,
                    Err(Error::DataQuality {
                        rejected_reports: 1,
                        unverified_reports: 0
                    })
                ),
                "{metrics:?}"
            );
        }
        let present_weather = tagged_settlement_dir("rejected", Some("present_weather"));
        assert!(
            settle(&present_weather, &["temp_high", "wind_speed"])
                .await
                .is_ok()
        );
        assert!(settle(&present_weather, &["rain_amt"]).await.is_err());
    }

    #[tokio::test]
    async fn wind_settlement_keeps_stations_with_no_usable_temperatures() {
        let directory = tagged_settlement_dir_with(
            "unverified",
            Some("temperature,dewpoint"),
            "",
            "unverified",
        );
        let rows = settle(&directory, &["wind_speed", "wind_direction"])
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].station_id, "KORD");
        assert_eq!(rows[0].wind_speed, Some(20));
        assert_eq!(rows[0].wind_direction, Some(90));
        assert_eq!(rows[0].temp_high, None);
        assert_eq!(rows[0].temp_low, None);
        assert_eq!(rows[0].humidity, None);
        assert!(settle(&directory, &["temp_high"]).await.is_err());
    }

    #[tokio::test]
    async fn temperature_problems_hold_temperature_and_humidity_but_not_wind_events() {
        let rejected = tagged_settlement_dir("rejected", Some("temperature,dewpoint"));
        for metrics in [
            &["temp_high"][..],
            &["humidity"],
            &["temp_low", "wind_speed"],
        ] {
            assert!(
                matches!(
                    settle(&rejected, metrics).await,
                    Err(Error::DataQuality { .. })
                ),
                "{metrics:?}"
            );
        }
        // An unverified report drops out of temperature and humidity
        // instead. Here that leaves two hours without a usable temperature.
        let unverified = tagged_settlement_dir("unverified", Some("temperature,dewpoint"));
        for metrics in [&["temp_high"][..], &["temp_low", "wind_speed"]] {
            let Err(Error::ObservationCoverage { stations, reason }) =
                settle(&unverified, metrics).await
            else {
                panic!("{metrics:?}: a gap over 90 minutes blocks");
            };
            assert_eq!(stations, ["KORD"]);
            assert!(reason.contains(&format!("KORD/{}", metrics[0])), "{reason}");
            assert!(!reason.contains("wind_speed"), "{reason}");
            assert!(reason.contains("1 unverified reports"), "{reason}");
        }
        assert_eq!(
            settle(&unverified, &["humidity"]).await.unwrap()[0].humidity,
            None,
            "humidity samples have the same gap"
        );
        for (status, directory) in [("rejected", &rejected), ("unverified", &unverified)] {
            let rows = settle(directory, &["wind_speed", "wind_direction"])
                .await
                .unwrap();
            assert_eq!(rows[0].wind_speed, Some(20), "{status}");
            assert_eq!(
                rows[0].temp_high,
                Some(11.0),
                "{status}: the flagged temperature stays out of every aggregate"
            );
        }
    }

    /// Event 01a0e83b was held by one report without unambiguous raw METAR
    /// temperature evidence among many validated ones.
    #[tokio::test]
    async fn an_unverified_report_drops_out_of_the_metrics_it_affects() {
        let directory = half_hourly_settlement_dir("unverified", Some("temperature"));
        let metrics = ["temp_high", "temp_low", "wind_speed"];
        let rows = settle(&directory, &metrics).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].temp_high, rows[0].temp_low),
            (Some(12.5), Some(11.0)),
            "the unverified 14 °C comes from no other report"
        );
        assert_eq!(rows[0].wind_speed, Some(20), "its wind is still used");
        let request = ObservationRequest {
            start: Some(time::macros::datetime!(2026-01-17 18:00 UTC)),
            end: Some(time::macros::datetime!(2026-01-17 20:00 UTC)),
            station_ids: "KORD".into(),
            temperature_unit: TemperatureUnit::Celsius,
        };
        let quality = access(&directory)
            .observation_quality(&request, vec!["KORD".into()], &metrics.map(String::from))
            .await
            .unwrap();
        assert_eq!(
            (quality.rejected_reports, quality.unverified_reports),
            (0, 1),
            "operators still see the report"
        );

        // Problems without tags affect every metric, wind included.
        let untagged = half_hourly_settlement_dir("unverified", None);
        let rows = settle(&untagged, &metrics).await.unwrap();
        assert_eq!(rows[0].temp_high, Some(12.5));
        assert_eq!(rows[0].wind_speed, Some(8));

        // A wind problem leaves the temperature in, and a wind gap leaves
        // wind missing, which the settlement check names.
        let wind = tagged_settlement_dir("unverified", Some("wind"));
        let rows = settle(&wind, &metrics).await.unwrap();
        assert_eq!(rows[0].temp_high, Some(14.0));
        assert_eq!(rows[0].wind_speed, None);

        // Rejected reports still hold the event.
        assert!(matches!(
            settle(
                &half_hourly_settlement_dir("rejected", Some("temperature")),
                &metrics
            )
            .await,
            Err(Error::DataQuality {
                rejected_reports: 1,
                unverified_reports: 0
            })
        ));
    }

    #[tokio::test]
    async fn untagged_or_unknown_problems_hold_every_event() {
        for tags in [None, Some(""), Some("precipitation,sunshine")] {
            let directory = tagged_settlement_dir("rejected", tags);
            assert!(
                matches!(
                    settle(&directory, &["temp_high", "wind_speed"]).await,
                    Err(Error::DataQuality {
                        rejected_reports: 1,
                        ..
                    })
                ),
                "{tags:?}"
            );
            let values = access(&directory)
                .observation_data(
                    &ObservationRequest {
                        start: Some(time::macros::datetime!(2026-01-17 18:00 UTC)),
                        end: Some(time::macros::datetime!(2026-01-17 19:59 UTC)),
                        station_ids: "KORD".into(),
                        temperature_unit: TemperatureUnit::Celsius,
                    },
                    vec!["KORD".into()],
                )
                .await
                .unwrap();
            assert_eq!(values[0].temp_high, 11.0, "{tags:?}");
            assert_eq!(values[0].wind_speed, Some(8), "{tags:?}");
        }
    }

    #[tokio::test]
    async fn quality_counts_follow_the_requested_metrics() {
        let directory = tagged_settlement_dir("rejected", Some("precipitation"));
        let request = ObservationRequest {
            start: Some(time::macros::datetime!(2026-01-17 18:00 UTC)),
            end: Some(time::macros::datetime!(2026-01-17 20:00 UTC)),
            station_ids: "KORD".into(),
            temperature_unit: TemperatureUnit::Celsius,
        };
        let weather = access(&directory);
        for (metrics, expected) in [
            (&["temp_high", "wind_direction"][..], 0),
            (&["rain_amt"], 1),
            (&[], 1),
        ] {
            let metrics: Vec<String> = metrics.iter().map(|m| m.to_string()).collect();
            let quality = weather
                .observation_quality(&request, vec!["KORD".into()], &metrics)
                .await
                .unwrap();
            assert_eq!(quality.rejected_reports, expected, "{metrics:?}");
        }
    }

    #[tokio::test]
    async fn newer_rejected_publication_cannot_fall_back_to_older_valid_report() {
        let good = "SELECT 'KPWM' AS station_id, '2026-01-17T18:51:00Z' AS generated_at,
                   18.9::DOUBLE AS temperature_value, 'celsius' AS temperature_unit_code,
                   'validated' AS quality_status, 'metar-consistency-v1' AS validation_version,
                   'METAR KPWM 171851Z 19/06 RMK T01890056' AS raw_text";
        let bad = good
            .replace("18.9::DOUBLE", "60.0::DOUBLE")
            .replace("'validated'", "'rejected'");
        let directory = quality_data_dir(&[
            ("observations_2026-01-17T19:00:00Z.parquet", good),
            ("observations_2026-01-17T20:00:00Z.parquet", &bad),
        ]);
        let request = ObservationRequest {
            station_ids: "KPWM".into(),
            ..day_request()
        };
        let weather = access(&directory);
        assert!(matches!(
            weather
                .settlement_observations(&request, request.station_ids(), request.end.unwrap(), &[])
                .await,
            Err(Error::DataQuality {
                rejected_reports: 1,
                ..
            })
        ));
        // A later explicitly validated correction replaces the bad publication.
        let corrected = quality_data_dir(&[
            ("observations_2026-01-17T19:00:00Z.parquet", &bad),
            ("observations_2026-01-17T20:00:00Z.parquet", good),
        ]);
        assert_eq!(
            access(&corrected)
                .observation_quality(&request, request.station_ids(), &[])
                .await
                .unwrap()
                .rejected_reports,
            0
        );
    }

    #[tokio::test]
    async fn explicit_unverified_reports_drop_out_and_unproven_empty_stations_cannot_settle() {
        let directory = quality_data_dir(&[(
            "observations_2026-01-17T21:00:00Z.parquet",
            "SELECT 'KPWM' AS station_id, '2026-01-17T18:51:00Z' AS generated_at,
                    18.9::DOUBLE AS temperature_value, 'celsius' AS temperature_unit_code,
                    'unverified' AS quality_status, 'metar-consistency-v1' AS validation_version",
        )]);
        let request = ObservationRequest {
            station_ids: "KPWM".into(),
            ..day_request()
        };
        let weather = access(&directory);
        assert!(
            weather
                .observation_data(&request, request.station_ids())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            weather
                .observation_quality(&request, request.station_ids(), &[])
                .await
                .unwrap()
                .unverified_reports,
            1
        );
        // The report drops out; nothing proves the station's coverage.
        assert!(matches!(
            weather
                .settlement_observations(&request, request.station_ids(), request.end.unwrap(), &[])
                .await,
            Err(Error::ObservationCoverage { .. })
        ));
        assert!(matches!(
            weather
                .settlement_observations(
                    &request,
                    vec!["KABSENT".into()],
                    request.end.unwrap(),
                    &[]
                )
                .await,
            Err(Error::ObservationCoverage { .. })
        ));
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
                 ('2026-01-17T18:00:00Z', 0.0),
                 ('2026-01-17T19:00:00Z', -5.0),
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

        assert_eq!(kord.latest_temp, Some(23.0));
        assert_ne!(kord.latest_temp, Some(kord.temp_low));
        assert_ne!(kord.latest_temp, Some(kord.temp_high));
        assert_eq!(kord.temp_high, 32.0);
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

    fn prepared_access(directory: &tempfile::TempDir) -> WeatherAccess {
        WeatherAccess::with_derived_forecasts(
            Arc::new(crate::file_access::FileAccess::new(
                directory.path().to_string_lossy().into_owned(),
            )),
            &directory.path().join("derived"),
        )
    }

    async fn copy_forecast(
        derived: DerivedForecasts,
        file: ParquetFileName,
        source: String,
    ) -> derived::Copied {
        tokio::task::spawn_blocking(move || derived.copy(&file, &source))
            .await
            .unwrap()
            .unwrap()
    }

    /// Copies every forecast file and folds every day, whatever their age
    /// (`prepare_files` only prepares recent ones). Returns the copies.
    async fn prepare_everything(access: &WeatherAccess) -> Vec<(String, derived::Copied)> {
        let derived = access.derived.clone().unwrap();
        let folds = access.folds.clone().unwrap();
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
        let mut copies = vec![];
        for name in names {
            let file = ParquetFileName::parse(&name).unwrap();
            let source = access.file_access.build_file_path(&file);
            let result = copy_forecast(derived.clone(), file.clone(), source).await;
            if let Some(copy) = derived.existing(&file) {
                copies.push((file, copy));
            }
            copied.push((name, result));
        }
        tokio::task::spawn_blocking(move || folds.fold_all(&copies))
            .await
            .unwrap()
            .unwrap();
        copied
    }

    #[tokio::test]
    async fn forecasts_match_the_previous_query_from_files_copies_and_folds() {
        let directory = forecast_data_dir();
        let published = access(&directory);
        let prepared_directory = forecast_data_dir();
        let prepared = prepared_access(&prepared_directory);
        let copied = prepare_everything(&prepared).await;
        assert_eq!(copied.len(), 6);
        assert!(
            copied
                .iter()
                .all(|(_, copied)| *copied == derived::Copied::Made)
        );
        let mut folded_requests = 0;
        for (request, station_ids) in fixture_forecast_requests() {
            let context = format!(
                "{:?}..{:?} issued {:?}..{:?} for {:?} in {}",
                request.start,
                request.end,
                request.generated_start,
                request.generated_end,
                station_ids,
                request.temperature_unit
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
            assert_same_forecasts(&expected, &from_files, &format!("published: {context}"));
            let from_prepared = prepared
                .forecasts_data(&request, station_ids.clone())
                .await
                .unwrap();
            assert_same_forecasts(
                &expected,
                &from_prepared,
                &format!("copies and folds: {context}"),
            );
            let names = prepared
                .file_access
                .grab_file_names(forecast_file_params(
                    forecast_generated_window(&request, OffsetDateTime::now_utc()),
                    OffsetDateTime::now_utc(),
                ))
                .await
                .unwrap();
            let window = forecast_generated_window(&request, OffsetDateTime::now_utc());
            if !prepared
                .folds
                .as_ref()
                .unwrap()
                .plan(names, window)
                .folds
                .is_empty()
            {
                folded_requests += 1;
            }
        }
        assert!(
            folded_requests > 0,
            "some fixture requests must read folds, not only copies"
        );
    }

    /// The spans of the folds a query reads, and the single files.
    fn planned(plan: folds::Plan) -> (Vec<String>, Vec<String>) {
        let span = |path: &String| {
            let name = path.rsplit('/').next().unwrap();
            name.rsplit_once('-').unwrap().0.to_owned()
        };
        let mut folds: Vec<_> = plan.folds.iter().map(span).collect();
        let mut files = plan.files;
        folds.sort();
        files.sort();
        (folds, files)
    }

    #[tokio::test]
    async fn a_fold_stands_in_only_for_files_wholly_inside_the_query() {
        let rows = |issued: &[&str]| {
            issued
                .iter()
                .map(|issued| {
                    format!(
                        "SELECT 'KTEST' AS station_id, '2026-01-18T00:00:00Z' AS begin_time,
                            '2026-01-18T12:00:00Z' AS end_time, '{issued}' AS generated_at,
                            30::BIGINT AS min_temp, 60::BIGINT AS max_temp,
                            'fahrenheit' AS temperature_unit_code"
                    )
                })
                .collect::<Vec<_>>()
                .join(" UNION ALL ")
        };
        let directory = data_dir(&[
            (
                "forecasts_2026-01-17T06:00:00Z.parquet",
                &rows(&["2026-01-17T06:02:00Z"]),
            ),
            (
                "forecasts_2026-01-17T12:00:00Z.parquet",
                &rows(&["2026-01-17T12:03:00Z", "2026-01-17T12:05:00Z"]),
            ),
            (
                "forecasts_2026-01-17T18:00:00Z.parquet",
                &rows(&["2026-01-17T18:04:00Z"]),
            ),
        ]);
        let access = prepared_access(&directory);
        prepare_everything(&access).await;
        let folds = access.folds.as_ref().unwrap();
        let file = |hour: &str| format!("forecasts_2026-01-17T{hour}:00:00Z.parquet");
        let names = || vec![file("06"), file("12"), file("18")];
        let at = |text: &str| OffsetDateTime::parse(text, &Rfc3339).unwrap();
        let day = |from: &str, to: &str| (at(from), at(to));
        // Every issue inside: the day's fold alone.
        assert_eq!(
            planned(folds.plan(names(), day("2026-01-17T00:00:00Z", "2026-01-18T00:00:00Z"))),
            (vec!["2026-01-17".to_owned()], vec![])
        );
        // Issues from 06:30: the later quarters' folds; the first file's
        // issues are all outside, so it is not read at all.
        assert_eq!(
            planned(folds.plan(names(), day("2026-01-17T06:30:00Z", "2026-01-18T00:00:00Z"))),
            (
                vec!["2026-01-17T12".to_owned(), "2026-01-17T18".to_owned()],
                vec![]
            )
        );
        // A window ending between a file's issues reads that file itself.
        assert_eq!(
            planned(folds.plan(names(), day("2026-01-17T00:00:00Z", "2026-01-17T12:04:00Z"))),
            (vec!["2026-01-17T06".to_owned()], vec![file("12")])
        );
        // A folded file outside the publication window rules its folds out.
        assert_eq!(
            planned(folds.plan(
                names()[1..].to_vec(),
                day("2026-01-17T00:00:00Z", "2026-01-18T00:00:00Z")
            )),
            (
                vec!["2026-01-17T12".to_owned(), "2026-01-17T18".to_owned()],
                vec![]
            )
        );
        // A file published after the folds were written is read beside them.
        let mut later = names();
        later.push(file("23"));
        assert_eq!(
            planned(folds.plan(later, day("2026-01-17T00:00:00Z", "2026-01-18T00:00:00Z"))),
            (vec!["2026-01-17".to_owned()], vec![file("23")])
        );
    }

    #[tokio::test]
    async fn folds_grow_with_each_upload_and_old_ones_are_pruned() {
        let row = |issued: &str, high: i64| {
            format!(
                "SELECT 'KTEST' AS station_id, '2026-01-18T00:00:00Z' AS begin_time,
                    '2026-01-18T12:00:00Z' AS end_time, '{issued}' AS generated_at,
                    30::BIGINT AS min_temp, {high}::BIGINT AS max_temp, 'fahrenheit' AS temperature_unit_code"
            )
        };
        let directory = data_dir(&[(
            "forecasts_2026-01-17T06:00:00Z.parquet",
            &row("2026-01-17T06:02:00Z", 60),
        )]);
        let access = prepared_access(&directory);
        prepare_everything(&access).await;
        let request = ForecastRequest {
            start: Some(time::macros::datetime!(2026-01-18 00:00 UTC)),
            end: Some(time::macros::datetime!(2026-01-19 00:00 UTC)),
            generated_start: Some(time::macros::datetime!(2026-01-17 00:00 UTC)),
            generated_end: Some(time::macros::datetime!(2026-01-18 00:00 UTC)),
            station_ids: "KTEST".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let high = |forecasts: Vec<Forecast>| forecasts[0].temp_high;
        assert_eq!(
            high(
                access
                    .forecasts_data(&request, vec!["KTEST".into()])
                    .await
                    .unwrap()
            ),
            60
        );

        // A newer issue arrives: read beside the fold until folded, then from it.
        let connection = open_connection().unwrap();
        connection
            .execute_batch(&format!(
                "COPY ({}) TO '{}' (FORMAT PARQUET)",
                row("2026-01-17T07:02:00Z", 70),
                directory
                    .path()
                    .join("2026-01-17/forecasts_2026-01-17T07:00:00Z.parquet")
                    .display()
            ))
            .unwrap();
        assert_eq!(
            high(
                access
                    .forecasts_data(&request, vec!["KTEST".into()])
                    .await
                    .unwrap()
            ),
            70
        );
        let root = directory
            .path()
            .join("derived")
            .join("folds-v2-native-intervals");
        let before: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        prepare_everything(&access).await;
        assert_eq!(
            high(
                access
                    .forecasts_data(&request, vec!["KTEST".into()])
                    .await
                    .unwrap()
            ),
            70
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("2026-01-17.json")).unwrap()).unwrap();
        assert_eq!(manifest["sources"].as_array().unwrap().len(), 2);
        // The replaced fold stays for readers that saw the old manifest.
        let files = |root: &Path| {
            std::fs::read_dir(root)
                .unwrap()
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".parquet"))
                .count()
        };
        // The day's fold and its first quarter's, each replaced once.
        assert_eq!(files(&root), 4, "{before:?}");
        access
            .folds
            .as_ref()
            .unwrap()
            .prune(time::macros::date!(2026 - 01 - 01))
            .unwrap();
        assert_eq!(files(&root), 4, "retired folds wait ten minutes");
        access
            .folds
            .as_ref()
            .unwrap()
            .prune(time::macros::date!(2026 - 01 - 18))
            .unwrap();
        assert_eq!(files(&root), 0, "days before the oldest kept day go");
        assert!(!root.join("2026-01-17.json").exists());
    }

    #[tokio::test]
    async fn local_days_group_forecasts_and_observations_by_the_readers_calendar() {
        // New York is UTC-5 until 2026-03-08 02:00 local, then UTC-4.
        let directory = data_dir(&[
            (
                "forecasts_2026-03-06T12:00:00Z.parquet",
                "SELECT 'KTEST' AS station_id, begin_time, end_time, '2026-03-06T12:00:00Z' AS generated_at,
                        30::BIGINT AS min_temp, max_temp::BIGINT AS max_temp, 'fahrenheit' AS temperature_unit_code
                 FROM (VALUES ('2026-03-07T03:00:00Z', '2026-03-07T04:00:00Z', 50),
                              ('2026-03-07T06:00:00Z', '2026-03-07T07:00:00Z', 60),
                              ('2026-03-09T04:30:00Z', '2026-03-09T05:30:00Z', 70))
                      AS periods(begin_time, end_time, max_temp)",
            ),
            (
                "observations_2026-03-09T06:00:00Z.parquet",
                "SELECT 'KTEST' AS station_id, generated_at, temperature_value::DOUBLE AS temperature_value,
                        'fahrenheit' AS temperature_unit_code
                 FROM (VALUES ('2026-03-07T03:00:00Z', 40), ('2026-03-07T06:00:00Z', 45),
                              ('2026-03-09T04:30:00Z', 55))
                      AS reports(generated_at, temperature_value)",
            ),
        ]);
        let access = access(&directory);
        let new_york = Calendar::from_zone_name("America/New_York").unwrap();
        let forecasts = ForecastRequest {
            start: Some(time::macros::datetime!(2026-03-06 00:00 UTC)),
            end: Some(time::macros::datetime!(2026-03-10 00:00 UTC)),
            generated_start: Some(time::macros::datetime!(2026-03-06 00:00 UTC)),
            generated_end: Some(time::macros::datetime!(2026-03-07 00:00 UTC)),
            station_ids: "KTEST".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let highs = |rows: Vec<Forecast>| {
            rows.into_iter()
                .map(|row| (row.date, row.temp_high))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            highs(
                access
                    .calendar_forecasts(&forecasts, vec!["KTEST".into()], new_york)
                    .await
                    .unwrap()
            ),
            vec![
                ("2026-03-06 00:00:00".to_owned(), 50),
                ("2026-03-07 00:00:00".to_owned(), 60),
                // 00:30 EDT on the 9th; a fixed EST offset would say the 8th.
                ("2026-03-09 00:00:00".to_owned(), 70),
            ]
        );
        assert_eq!(
            highs(
                access
                    .calendar_forecasts(&forecasts, vec!["KTEST".into()], Calendar::Utc)
                    .await
                    .unwrap()
            ),
            highs(
                access
                    .forecasts_data(&forecasts, vec!["KTEST".into()])
                    .await
                    .unwrap()
            ),
        );
        let observations = ObservationRequest {
            start: forecasts.start,
            end: forecasts.end,
            station_ids: "KTEST".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let mut daily = access
            .calendar_daily_observations(&observations, vec!["KTEST".into()], new_york)
            .await
            .unwrap();
        daily.sort_by(|a, b| a.date.cmp(&b.date));
        assert_eq!(
            daily
                .iter()
                .map(|day| (day.date.as_str(), day.temp_high.round() as i64))
                .collect::<Vec<_>>(),
            vec![
                ("2026-03-06 00:00:00", 40),
                ("2026-03-07 00:00:00", 45),
                ("2026-03-09 00:00:00", 55),
            ]
        );
    }

    /// Forecasts as the daemon writes them: a low for each night and a high
    /// for each daytime on their own periods, winds as point samples.
    /// KWST's periods are a Pacific station's (highs 15:00–03:00 UTC, lows
    /// 03:00–16:00), KEST's an Eastern one's (12:00–00:00 and 00:00–13:00).
    /// KEST has no wind.
    fn native_forecast_dir() -> tempfile::TempDir {
        data_dir(&[(
            "forecasts_2026-10-05T11:05:00Z.parquet",
            "SELECT station_id, begin_time, end_time, '2026-10-05T11:00:00Z' AS generated_at,
                    min_temp::BIGINT AS min_temp, max_temp::BIGINT AS max_temp,
                    'fahrenheit' AS temperature_unit_code, wind_speed::BIGINT AS wind_speed,
                    (wind_speed * 10)::BIGINT AS wind_direction,
                    CASE WHEN begin_time = end_time THEN 'instant' ELSE 'period' END AS interval_kind
             FROM (VALUES
                 ('KWST', '2026-10-05T03:00:00Z', '2026-10-05T16:00:00Z', 55, NULL, NULL),
                 ('KWST', '2026-10-05T15:00:00Z', '2026-10-06T03:00:00Z', NULL, 98, NULL),
                 ('KWST', '2026-10-06T03:00:00Z', '2026-10-06T16:00:00Z', 59, NULL, NULL),
                 ('KWST', '2026-10-06T15:00:00Z', '2026-10-07T03:00:00Z', NULL, 100, NULL),
                 ('KWST', '2026-10-07T03:00:00Z', '2026-10-07T16:00:00Z', 57, NULL, NULL),
                 ('KWST', '2026-10-07T15:00:00Z', '2026-10-08T03:00:00Z', NULL, 96, NULL),
                 ('KEST', '2026-10-05T00:00:00Z', '2026-10-05T13:00:00Z', 48, NULL, NULL),
                 ('KEST', '2026-10-05T12:00:00Z', '2026-10-06T00:00:00Z', NULL, 64, NULL),
                 ('KEST', '2026-10-06T00:00:00Z', '2026-10-06T13:00:00Z', 50, NULL, NULL),
                 ('KEST', '2026-10-06T12:00:00Z', '2026-10-07T00:00:00Z', NULL, 66, NULL),
                 ('KEST', '2026-10-07T00:00:00Z', '2026-10-07T13:00:00Z', 52, NULL, NULL),
                 ('KEST', '2026-10-07T12:00:00Z', '2026-10-08T00:00:00Z', NULL, 68, NULL),
                 ('KWST', '2026-10-05T12:00:00Z', '2026-10-05T12:00:00Z', NULL, NULL, 6),
                 ('KWST', '2026-10-05T15:00:00Z', '2026-10-05T15:00:00Z', NULL, NULL, 9),
                 ('KWST', '2026-10-05T18:00:00Z', '2026-10-05T18:00:00Z', NULL, NULL, 12),
                 ('KWST', '2026-10-05T21:00:00Z', '2026-10-05T21:00:00Z', NULL, NULL, 8),
                 ('KWST', '2026-10-06T00:00:00Z', '2026-10-06T00:00:00Z', NULL, NULL, 5),
                 ('KWST', '2026-10-06T03:00:00Z', '2026-10-06T03:00:00Z', NULL, NULL, 4),
                 ('KWST', '2026-10-06T06:00:00Z', '2026-10-06T06:00:00Z', NULL, NULL, 3),
                 ('KWST', '2026-10-06T09:00:00Z', '2026-10-06T09:00:00Z', NULL, NULL, 4),
                 ('KWST', '2026-10-06T12:00:00Z', '2026-10-06T12:00:00Z', NULL, NULL, 7),
                 ('KWST', '2026-10-06T15:00:00Z', '2026-10-06T15:00:00Z', NULL, NULL, 11),
                 ('KWST', '2026-10-06T18:00:00Z', '2026-10-06T18:00:00Z', NULL, NULL, 14),
                 ('KWST', '2026-10-06T21:00:00Z', '2026-10-06T21:00:00Z', NULL, NULL, 10),
                 ('KWST', '2026-10-07T00:00:00Z', '2026-10-07T00:00:00Z', NULL, NULL, 6),
                 ('KWST', '2026-10-07T03:00:00Z', '2026-10-07T03:00:00Z', NULL, NULL, 5),
                 ('KWST', '2026-10-07T12:00:00Z', '2026-10-07T12:00:00Z', NULL, NULL, 8)
             ) AS periods(station_id, begin_time, end_time, min_temp, max_temp, wind_speed)",
        )])
    }

    /// A request for the native fixture's stations from its one issue.
    fn native_forecast_request(start: &str, end: &str) -> ForecastRequest {
        let at = |text: &str| OffsetDateTime::parse(text, &Rfc3339).unwrap();
        ForecastRequest {
            start: Some(at(start)),
            end: Some(at(end)),
            generated_start: Some(at("2026-10-01T00:00:00Z")),
            generated_end: Some(at("2026-10-05T12:00:00Z")),
            station_ids: "KEST,KWST".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        }
    }

    /// Station, day, low, high and wind speed of each row.
    fn day_values(forecasts: &[Forecast]) -> Vec<(&str, &str, i64, i64, Option<i64>)> {
        forecasts
            .iter()
            .map(|row| {
                (
                    row.station_id.as_str(),
                    &row.date[..10],
                    row.temp_low,
                    row.temp_high,
                    row.wind_speed,
                )
            })
            .collect()
    }

    #[test]
    fn only_the_first_and_last_day_of_a_window_can_be_partial() {
        let days = |start: &str, end: &str, calendar: Calendar| {
            let format = |time: OffsetDateTime| time.format(&Rfc3339).unwrap();
            partial_days(&native_forecast_request(start, end), calendar)
                .into_iter()
                .map(|day| (day.date, format(day.start), format(day.end)))
                .collect::<Vec<_>>()
        };
        let day = |date: &str, start: &str, end: &str| {
            (format!("{date} 00:00:00"), start.to_owned(), end.to_owned())
        };
        assert!(
            days(
                "2026-10-06T00:00:00Z",
                "2026-10-07T00:00:00Z",
                Calendar::Utc
            )
            .is_empty()
        );
        assert!(
            days(
                "2026-10-06T00:00:00Z",
                "2026-10-09T00:00:00Z",
                Calendar::Utc
            )
            .is_empty()
        );
        assert_eq!(
            days(
                "2026-10-05T18:25:00Z",
                "2026-10-06T18:25:00Z",
                Calendar::Utc
            ),
            vec![
                day("2026-10-05", "2026-10-05T18:25:00Z", "2026-10-06T00:00:00Z"),
                day("2026-10-06", "2026-10-06T00:00:00Z", "2026-10-06T18:25:00Z"),
            ]
        );
        assert_eq!(
            days(
                "2026-10-06T00:00:00Z",
                "2026-10-06T12:00:00Z",
                Calendar::Utc
            ),
            vec![day(
                "2026-10-06",
                "2026-10-06T00:00:00Z",
                "2026-10-06T12:00:00Z"
            )]
        );
        assert_eq!(
            days(
                "2026-10-05T12:00:00Z",
                "2026-10-09T00:00:00Z",
                Calendar::Utc
            ),
            vec![day(
                "2026-10-05",
                "2026-10-05T12:00:00Z",
                "2026-10-06T00:00:00Z"
            )]
        );
        // A UTC day is two partial days in New York (UTC-4 in October).
        let new_york = Calendar::from_zone_name("America/New_York").unwrap();
        assert_eq!(
            days("2026-10-06T00:00:00Z", "2026-10-07T00:00:00Z", new_york),
            vec![
                day("2026-10-05", "2026-10-06T00:00:00Z", "2026-10-06T04:00:00Z"),
                day("2026-10-06", "2026-10-06T04:00:00Z", "2026-10-07T00:00:00Z"),
            ]
        );
        assert!(days("2026-10-06T04:00:00Z", "2026-10-07T04:00:00Z", new_york).is_empty());
        let unbounded = ForecastRequest {
            end: None,
            ..native_forecast_request("2026-10-05T18:25:00Z", "2026-10-06T18:25:00Z")
        };
        assert!(partial_days(&unbounded, Calendar::Utc).is_empty());
    }

    #[tokio::test]
    async fn a_forecast_window_has_a_row_for_every_utc_day_it_overlaps() {
        let directory = native_forecast_dir();
        let access = access(&directory);
        type Day = (&'static str, &'static str, i64, i64, Option<i64>);
        let cases: [(&str, &str, &str, &[Day]); 8] = [
            (
                "starts mid-day",
                "2026-10-05T18:25:00Z",
                "2026-10-06T18:25:00Z",
                &[
                    // The evening has a high of its own and borrows the
                    // coming night's low.
                    ("KEST", "2026-10-05", 50, 64, None),
                    ("KEST", "2026-10-06", 50, 66, None),
                    ("KWST", "2026-10-05", 59, 98, Some(8)),
                    ("KWST", "2026-10-06", 59, 100, Some(14)),
                ],
            ),
            (
                "ends mid-day",
                "2026-10-06T00:00:00Z",
                "2026-10-07T12:00:00Z",
                &[
                    ("KEST", "2026-10-06", 50, 66, None),
                    // The morning borrows the window's last high, not the
                    // one of the afternoon after the window.
                    ("KEST", "2026-10-07", 52, 66, None),
                    ("KWST", "2026-10-06", 59, 100, Some(14)),
                    ("KWST", "2026-10-07", 57, 100, Some(6)),
                ],
            ),
            (
                "spans midnight from its last half hour",
                "2026-10-05T23:30:00Z",
                "2026-10-06T23:30:00Z",
                &[
                    ("KEST", "2026-10-05", 50, 64, None),
                    ("KEST", "2026-10-06", 50, 66, None),
                    // No wind sample falls in the half hour; midnight's is nearest.
                    ("KWST", "2026-10-05", 59, 98, Some(5)),
                    ("KWST", "2026-10-06", 59, 100, Some(14)),
                ],
            ),
            (
                "00:00 to 12:00",
                "2026-10-06T00:00:00Z",
                "2026-10-06T12:00:00Z",
                &[
                    // No high overlaps an Eastern night; the day's own is
                    // as near as the day before's, and later.
                    ("KEST", "2026-10-06", 50, 66, None),
                    // The evening's high runs three hours into the window.
                    ("KWST", "2026-10-06", 59, 98, Some(5)),
                ],
            ),
            (
                "12:00 to 24:00",
                "2026-10-06T12:00:00Z",
                "2026-10-07T00:00:00Z",
                &[
                    ("KEST", "2026-10-06", 50, 66, None),
                    ("KWST", "2026-10-06", 59, 100, Some(14)),
                ],
            ),
            (
                "exactly one day",
                "2026-10-06T00:00:00Z",
                "2026-10-07T00:00:00Z",
                &[
                    ("KEST", "2026-10-06", 50, 66, None),
                    ("KWST", "2026-10-06", 59, 100, Some(14)),
                ],
            ),
            (
                "12 hours across midnight",
                "2026-10-05T18:00:00Z",
                "2026-10-06T06:00:00Z",
                &[
                    ("KEST", "2026-10-05", 50, 64, None),
                    ("KEST", "2026-10-06", 50, 64, None),
                    ("KWST", "2026-10-05", 59, 98, Some(12)),
                    ("KWST", "2026-10-06", 59, 98, Some(5)),
                ],
            ),
            (
                "whole days borrow nothing",
                "2026-10-06T00:00:00Z",
                "2026-10-08T00:00:00Z",
                &[
                    ("KEST", "2026-10-06", 50, 66, None),
                    ("KEST", "2026-10-07", 52, 68, None),
                    ("KWST", "2026-10-06", 59, 100, Some(14)),
                    ("KWST", "2026-10-07", 57, 96, Some(8)),
                ],
            ),
        ];
        for (name, start, end, expected) in cases {
            let request = native_forecast_request(start, end);
            let forecasts = access
                .forecasts_data(&request, request.station_ids())
                .await
                .unwrap();
            assert_eq!(day_values(&forecasts), expected, "{name}");
            for row in &forecasts {
                let (from, to) = (
                    OffsetDateTime::parse(&row.start_time, &Rfc3339).unwrap(),
                    OffsetDateTime::parse(&row.end_time, &Rfc3339).unwrap(),
                );
                assert!(
                    request.start.unwrap() <= from && from <= to && to <= request.end.unwrap(),
                    "{name}: {row:?} lies outside the window"
                );
            }
        }
    }

    /// The two windows that returned too little on 2026-10-05: the entry
    /// form's (a day from 18:25 UTC, issues before the start, as the
    /// coordinator asks) lost its first day, and discovery's 00:00 to 12:00
    /// (no issue bounds) returned no rows at all.
    #[tokio::test]
    async fn windows_cut_between_a_days_high_and_low_keep_their_rows() {
        let directory = native_forecast_dir();
        let access = access(&directory);
        let start = time::macros::datetime!(2026-10-05 18:25 UTC);
        let entry_form = ForecastRequest {
            generated_start: Some(start - Duration::days(7)),
            generated_end: Some(start - Duration::nanoseconds(1)),
            ..native_forecast_request("2026-10-05T18:25:00Z", "2026-10-06T18:25:00Z")
        };
        let forecasts = access
            .forecasts_data(&entry_form, entry_form.station_ids())
            .await
            .unwrap();
        for station in ["KEST", "KWST"] {
            let days: Vec<_> = forecasts
                .iter()
                .filter(|row| row.station_id == station)
                .collect();
            assert_eq!(
                days.iter().map(|row| &row.date[..10]).collect::<Vec<_>>(),
                ["2026-10-05", "2026-10-06"],
                "{station}: one row for each UTC day of the window"
            );
            assert_eq!(days[0].start_time, "2026-10-05T18:25:00.000000Z");
            assert_eq!(days[0].end_time, "2026-10-06T00:00:00.000000Z");
            assert_eq!(days[1].start_time, "2026-10-06T00:00:00.000000Z");
            assert_eq!(days[1].end_time, "2026-10-06T18:25:00.000000Z");
        }
        // The days combine as the coordinator combines them.
        let west: Vec<_> = forecasts
            .iter()
            .filter(|row| row.station_id == "KWST")
            .collect();
        assert_eq!(west.iter().map(|row| row.temp_high).max(), Some(100));
        assert_eq!(west.iter().map(|row| row.temp_low).min(), Some(59));
        assert_eq!(west.iter().filter_map(|row| row.wind_speed).max(), Some(14));
        assert!(west.iter().all(|row| row.wind_speed.is_some()));

        let discovery = ForecastRequest {
            generated_start: None,
            generated_end: None,
            ..native_forecast_request("2026-10-06T00:00:00Z", "2026-10-06T12:00:00Z")
        };
        let forecasts = access
            .forecasts_data(&discovery, discovery.station_ids())
            .await
            .unwrap();
        assert_eq!(
            day_values(&forecasts),
            [
                ("KEST", "2026-10-06", 50, 66, None),
                ("KWST", "2026-10-06", 59, 98, Some(5)),
            ]
        );
        assert_eq!(forecasts[0].start_time, "2026-10-06T00:00:00.000000Z");
        assert_eq!(forecasts[0].end_time, "2026-10-06T12:00:00.000000Z");
    }

    #[tokio::test]
    async fn a_borrowed_extreme_is_near_and_never_inverts_the_day() {
        let directory = data_dir(&[(
            "forecasts_2026-10-05T11:05:00Z.parquet",
            "SELECT station_id, begin_time, end_time, '2026-10-05T11:00:00Z' AS generated_at,
                    min_temp::BIGINT AS min_temp, max_temp::BIGINT AS max_temp,
                    'fahrenheit' AS temperature_unit_code
             FROM (VALUES
                 ('KCLD', '2026-10-06T00:00:00Z', '2026-10-06T13:00:00Z', 60, NULL),
                 ('KCLD', '2026-10-06T12:00:00Z', '2026-10-07T00:00:00Z', NULL, 55),
                 ('KFAR', '2026-10-06T00:00:00Z', '2026-10-06T06:00:00Z', 40, NULL),
                 ('KFAR', '2026-10-07T00:00:01Z', '2026-10-07T12:00:00Z', NULL, 70)
             ) AS periods(station_id, begin_time, end_time, min_temp, max_temp)",
        )]);
        let request = ForecastRequest {
            station_ids: "KCLD,KFAR".into(),
            ..native_forecast_request("2026-10-06T00:00:00Z", "2026-10-06T12:00:00Z")
        };
        let forecasts = access(&directory)
            .forecasts_data(&request, request.station_ids())
            .await
            .unwrap();
        // KCLD's afternoon is colder than its night: the borrowed high
        // stops at the low. KFAR's next high begins a second out of reach.
        assert_eq!(
            day_values(&forecasts),
            [("KCLD", "2026-10-06", 60, 60, None)]
        );
    }

    /// The previous query, the current query over the published files, and
    /// the current query over copies and folds, on a copy of real data:
    ///
    /// ```text
    /// ORACLE_PERF_DATA=~/weather cargo test -p oracle --lib real_forecasts -- --ignored --nocapture
    /// ```
    ///
    /// Requests are those the pages and the coordinator make, as of the
    /// newest file. Local days have no previous query; they compare the
    /// published files with the copies and folds.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs ORACLE_PERF_DATA"]
    async fn real_forecasts_match_the_previous_query() {
        let directory = std::env::var("ORACLE_PERF_DATA")
            .expect("ORACLE_PERF_DATA must name a weather directory");
        let file_access = Arc::new(crate::file_access::FileAccess::new(directory.clone()));
        let published = WeatherAccess::new(file_access.clone());
        let prepared = WeatherAccess::with_derived_forecasts(
            file_access,
            &Path::new(&directory).join("derived"),
        );
        let started = std::time::Instant::now();
        prepared
            .prepare_files(&CancellationToken::new())
            .await
            .unwrap();
        println!(
            "\ncopies and folds ready in {:.1}s",
            started.elapsed().as_secs_f64()
        );
        let now = OffsetDateTime::now_utc();
        let today = now.replace_time(time::Time::MIDNIGHT);
        let airports: Vec<String> = "KATL,KLAX,KORD,KDFW,KDEN,KJFK,KSFO,KSEA,KLAS,KMCO,KEWR,KMIA,KPHX,KIAH,KBOS,KMSP,KFLL,KDTW,KPHL,KLGA,KBWI,KSLC,KDCA,KSAN,KTPA,KPDX,KSTL,KHNL,KBNA,KAUS,KMCI,KRDU,KMKE,KSMF,KCLT,KPIT,KSAT,KOAK,KCLE,KSJC,KIND,KCVG,KCMH,KJAN,KRSW,KABQ,KANC,KOMA,KBUF,KPBI,KBDL,KPVD,KBTV,KPWM,KMHT,KBOI,KBIL,KFSD,KFAR,KGEG,KICT,KLIT,KLEX,KBHM,KMEM,KJAX,KCHS,KRIC,KORF,KCRW,KPNS,KMOB,KSHV,KMSY,KTUL,KELP,KTUS,KCOS,KGRR,KDSM,KMSN,KDLH,KBZN,KGJT,KRAP,KFCA,KCYS,KJAR,KSGF,KFSM"
            .split(',')
            .map(String::from)
            .collect();
        let three: Vec<String> = ["KPWM", "KBTV", "KBED"].map(String::from).to_vec();
        let one = vec!["KORD".to_owned()];
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
        let new_york = Calendar::from_zone_name("America/New_York").unwrap();
        let local_day = new_york.start_of_day(now);
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
            (
                "admin page: 90 airports, next 2 days",
                request(now, now + Duration::days(2), None, &airports),
            ),
        ];
        println!(
            "\n| Query | Previous (ms) | Current, files (ms) | Current, copies and folds (ms) | Rows |"
        );
        println!("| --- | ---: | ---: | ---: | ---: |");
        let timed = |started: std::time::Instant| started.elapsed().as_secs_f64() * 1000.0;
        for (name, (request, station_ids)) in cases {
            let started = std::time::Instant::now();
            let expected = sorted(
                legacy::forecasts_data(&published, &request, station_ids.clone())
                    .await
                    .unwrap_or_else(|error| panic!("previous query, {name}: {error}")),
            );
            let legacy_ms = timed(started);
            assert!(!expected.is_empty(), "{name}: no forecasts in the data");
            let started = std::time::Instant::now();
            let from_files = published
                .forecasts_data(&request, station_ids.clone())
                .await
                .unwrap();
            let files_ms = timed(started);
            let started = std::time::Instant::now();
            let from_prepared = prepared
                .forecasts_data(&request, station_ids.clone())
                .await
                .unwrap();
            let prepared_ms = timed(started);
            assert_same_forecasts(&expected, &from_files, name);
            assert_same_forecasts(&expected, &from_prepared, name);
            println!(
                "| {name} | {legacy_ms:.0} | {files_ms:.0} | {prepared_ms:.0} | {} |",
                expected.len()
            );
        }
        let (request, station_ids) = request(
            local_day,
            local_day + Duration::days(1),
            Some((local_day - Duration::days(1), before(local_day))),
            &airports,
        );
        let started = std::time::Instant::now();
        let from_files = published
            .calendar_forecasts(&request, station_ids.clone(), new_york)
            .await
            .unwrap();
        let files_ms = timed(started);
        let started = std::time::Instant::now();
        let from_prepared = prepared
            .calendar_forecasts(&request, station_ids, new_york)
            .await
            .unwrap();
        let prepared_ms = timed(started);
        assert!(!from_files.is_empty());
        assert_same_forecasts(&from_files, &from_prepared, "New York dashboard");
        println!(
            "| dashboard: 90 airports, New York day (no previous query) | - | {files_ms:.0} | {prepared_ms:.0} | {} |",
            from_files.len()
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
