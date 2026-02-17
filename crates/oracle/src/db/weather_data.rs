use crate::{
    file_access, FileAccess, FileData, FileParams, ForecastRequest, ObservationRequest,
    TemperatureUnit,
};
use async_trait::async_trait;
use duckdb::{
    arrow::array::{Array, Float64Array, Int64Array, RecordBatch, StringArray},
    params_from_iter, Connection,
};
use regex::Regex;
use scooby::postgres::Select;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime, Time};
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
    /// Get daily aggregated observations (grouped by UTC date)
    async fn daily_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<DailyObservation>, Error>;
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

        // Build station filter clause
        let station_filter = if !station_ids.is_empty() {
            let quoted: Vec<String> = station_ids.iter().map(|s| format!("'{}'", s)).collect();
            format!("WHERE station_id IN ({})", quoted.join(", "))
        } else {
            String::new()
        };

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
                    SELECT * FROM read_parquet(['{}'], union_by_name = true)
                )
            ),
            -- Deduplicate: for each station + time window, take the most recent forecast
            deduped_forecasts AS (
                SELECT DISTINCT ON (station_id, begin_time, end_time)
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
                ORDER BY station_id, begin_time, end_time, generated_at DESC
            ),
            -- Precipitation bucketing: rows exist at multiple interval durations (1h, 3h, 6h, 12h, 24h)
            -- Precipitation rows with duration info. Each precip field (QPF, snow, ice)
            -- may have a different native interval from NOAA, so we detect intervals per-field.
            precip_rows AS (
                SELECT
                    station_id,
                    DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT AS date,
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
                    DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT AS date,
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
                GROUP BY station_id, DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT
            )
            SELECT
                df.station_id,
                df.date,
                {} AS start_time,
                {} AS end_time,
                MIN(df.temp_low) AS temp_low,
                MAX(df.temp_high) AS temp_high,
                MAX(df.wind_speed) AS wind_speed,
                MAX(df.wind_direction) AS wind_direction,
                MAX(df.humidity_max) AS humidity_max,
                MIN(df.humidity_min) AS humidity_min,
                MAX(df.temperature_unit_code) AS temperature_unit_code,
                MAX(df.precip_chance) AS precip_chance,
                -- Calculate rain: QPF - (snow / snow_ratio) - ice
                -- If no snow_ratio, treat all QPF as rain (minus ice)
                -- Never return negative values
                GREATEST(0, COALESCE(
                    dp.total_qpf - (dp.snow_amt / NULLIF(dp.avg_snow_ratio, 0)) - COALESCE(dp.ice_amt, 0),
                    dp.total_qpf - COALESCE(dp.ice_amt, 0)
                )) AS rain_amt,
                dp.snow_amt AS snow_amt,
                dp.ice_amt AS ice_amt
            FROM daily_forecasts df
            LEFT JOIN daily_precip dp ON df.station_id = dp.station_id AND df.date = dp.date
            GROUP BY df.station_id, df.date, dp.total_qpf, dp.snow_amt, dp.avg_snow_ratio, dp.ice_amt
            "#,
            file_paths.join("', '"),
            station_filter,
            time_filter,
            start_time_expr,
            end_time_expr,
        );

        // Execute raw SQL directly
        let conn = self.open_connection()?;
        let mut stmt = conn.prepare(&query_sql)?;
        let records: Vec<RecordBatch> = stmt.query_arrow([])?.collect();

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

        if file_paths.is_empty() {
            return Ok(vec![]);
        }

        // Build station filter clause
        let station_filter = if !station_ids.is_empty() {
            let quoted: Vec<String> = station_ids.iter().map(|s| format!("'{}'", s)).collect();
            format!("WHERE station_id IN ({})", quoted.join(", "))
        } else {
            String::new()
        };

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
        // Precipitation is split into rain/snow/ice using wx_string (METAR weather codes):
        //   Snow: SN, BLSN, DRSN  |  Ice: FZRA, FZDZ, PL, GR, GS, IC  |  Rain: everything else
        // For old files without wx_string, temperature heuristic is used (<=2°C = snow)
        // precip_in is liquid equivalent; snow inches = precip_in * snow_ratio (default 10)
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
                    SELECT * FROM read_parquet(['{}'], union_by_name = true)
                )
                {} {}
            ),
            -- Classify each observation's precipitation type
            classified AS (
                SELECT *,
                    CASE
                        -- wx_string available: use METAR weather codes
                        WHEN wx_string IS NOT NULL AND wx_string != '' THEN
                            CASE
                                WHEN regexp_matches(wx_string, '(^|\s)(SN|BLSN|DRSN)(\s|$)') THEN 'snow'
                                WHEN regexp_matches(wx_string, '(^|\s)(FZRA|FZDZ|PL|GR|GS|IC)(\s|$)') THEN 'ice'
                                ELSE 'rain'
                            END
                        -- No wx_string: fall back to temperature heuristic
                        WHEN temperature_value IS NOT NULL AND temperature_value <= 2.0 THEN 'snow'
                        ELSE 'rain'
                    END AS precip_type
                FROM parquet_data
            )
            SELECT
                station_id,
                {} AS start_time,
                {} AS end_time,
                MIN(temperature_value) AS temp_low,
                MAX(temperature_value) AS temp_high,
                MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500) AS wind_speed,
                MAX(temperature_unit_code) AS temperature_unit_code,
                MAX(wind_direction) FILTER (WHERE wind_direction IS NOT NULL AND wind_direction >= 0 AND wind_direction <= 360) AS wind_direction,
                -- Derive humidity from temperature and dewpoint using Magnus formula
                CASE
                    WHEN AVG(dewpoint_value) IS NOT NULL AND AVG(temperature_value) IS NOT NULL
                    THEN ROUND(100.0 * EXP((17.625 * AVG(dewpoint_value)) / (243.04 + AVG(dewpoint_value)))
                         / EXP((17.625 * AVG(temperature_value)) / (243.04 + AVG(temperature_value))))::BIGINT
                    ELSE NULL
                END AS humidity,
                -- Rain: sum precip_in where type is rain (already liquid inches)
                SUM(precip_in) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'rain') AS rain_amt,
                -- Snow: precip_in * 10 (default snow ratio) to convert liquid equivalent to snow inches
                SUM(precip_in * 10.0) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'snow') AS snow_amt,
                -- Ice: liquid equivalent inches (roughly 1:1)
                SUM(precip_in) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'ice') AS ice_amt
            FROM classified
            GROUP BY station_id
            "#,
            file_paths.join("', '"),
            station_filter,
            time_filter,
            start_time_expr,
            end_time_expr,
        );

        let conn = self.open_connection()?;
        let mut stmt = conn.prepare(&query_sql)?;
        let records: Vec<RecordBatch> = stmt.query_arrow([])?.collect();
        let observations: Observations = records
            .iter()
            .map(|record| Observations::from_with_temp_unit(record, &req.temperature_unit))
            .fold(Observations::new(), |mut acc, obs| {
                acc.merge(obs);
                acc
            });
        Ok(observations.values)
    }

    async fn daily_observations(
        &self,
        req: &ObservationRequest,
        station_ids: Vec<String>,
    ) -> Result<Vec<DailyObservation>, Error> {
        let mut file_params: FileParams = req.into();
        if let Some(start_date) = req.start {
            file_params.start = Some(start_date.saturating_sub(Duration::days(1)));
        }
        let parquet_files = self.file_access.grab_file_names(file_params).await?;
        let file_paths = self.file_access.build_file_paths(parquet_files);

        if file_paths.is_empty() {
            return Ok(vec![]);
        }

        // Build station filter clause
        let station_filter = if !station_ids.is_empty() {
            let quoted: Vec<String> = station_ids.iter().map(|s| format!("'{}'", s)).collect();
            format!("WHERE station_id IN ({})", quoted.join(", "))
        } else {
            String::new()
        };

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
                    SELECT * FROM read_parquet(['{}'], union_by_name = true)
                )
                {} {}
            ),
            classified AS (
                SELECT *,
                    CASE
                        WHEN wx_string IS NOT NULL AND wx_string != '' THEN
                            CASE
                                WHEN regexp_matches(wx_string, '(^|\s)(SN|BLSN|DRSN)(\s|$)') THEN 'snow'
                                WHEN regexp_matches(wx_string, '(^|\s)(FZRA|FZDZ|PL|GR|GS|IC)(\s|$)') THEN 'ice'
                                ELSE 'rain'
                            END
                        WHEN temperature_value IS NOT NULL AND temperature_value <= 2.0 THEN 'snow'
                        ELSE 'rain'
                    END AS precip_type
                FROM parquet_data
            )
            SELECT
                station_id,
                DATE_TRUNC('day', generated_at::TIMESTAMP)::TEXT AS date,
                MIN(temperature_value) FILTER (WHERE temperature_value IS NOT NULL) AS temp_low,
                MAX(temperature_value) FILTER (WHERE temperature_value IS NOT NULL) AS temp_high,
                MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500) AS wind_speed,
                MAX(temperature_unit_code) AS temperature_unit_code,
                MAX(wind_direction) FILTER (WHERE wind_direction IS NOT NULL AND wind_direction >= 0 AND wind_direction <= 360) AS wind_direction,
                CASE
                    WHEN AVG(dewpoint_value) IS NOT NULL AND AVG(temperature_value) IS NOT NULL
                    THEN ROUND(100.0 * EXP((17.625 * AVG(dewpoint_value)) / (243.04 + AVG(dewpoint_value)))
                         / EXP((17.625 * AVG(temperature_value)) / (243.04 + AVG(temperature_value))))::BIGINT
                    ELSE NULL
                END AS humidity,
                SUM(precip_in) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'rain') AS rain_amt,
                SUM(precip_in * 10.0) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'snow') AS snow_amt,
                SUM(precip_in) FILTER (WHERE precip_in IS NOT NULL AND precip_in >= 0 AND precip_type = 'ice') AS ice_amt
            FROM classified
            GROUP BY station_id, DATE_TRUNC('day', generated_at::TIMESTAMP)::TEXT
            "#,
            file_paths.join("', '"),
            station_filter,
            time_filter,
        );

        let conn = self.open_connection()?;
        let mut stmt = conn.prepare(&query_sql)?;
        let records: Vec<RecordBatch> = stmt.query_arrow([])?.collect();
        let observations: DailyObservations = records
            .iter()
            .map(|record| DailyObservations::from_with_temp_unit(record, &req.temperature_unit))
            .fold(DailyObservations::new(), |mut acc, obs| {
                acc.merge(obs);
                acc
            });
        Ok(observations.values)
    }

    async fn stations(&self) -> Result<Vec<Station>, Error> {
        // Query all available observation files to find station data
        // Using None for start/end finds all available data
        let parquet_files = self
            .file_access
            .grab_file_names(FileParams {
                start: None,
                end: None,
                observations: Some(true),
                forecasts: Some(false),
            })
            .await?;
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
                station_id,
                COALESCE(station_name, '') AS station_name,
                COALESCE(state, '') AS state,
                COALESCE(iata_id, '') AS iata_id,
                elevation_m,
                latitude,
                longitude
            FROM (
                SELECT NULL::VARCHAR AS station_id, NULL::VARCHAR AS station_name,
                       NULL::VARCHAR AS state, NULL::VARCHAR AS iata_id,
                       NULL::DOUBLE AS elevation_m, NULL::DOUBLE AS latitude, NULL::DOUBLE AS longitude
                WHERE false
                UNION ALL BY NAME
                SELECT * FROM read_parquet(['{}'], union_by_name = true)
            )
            "#,
            file_paths.join("', '")
        );

        // Execute raw SQL directly since we're not using the scooby builder
        let conn = self.open_connection()?;
        let mut stmt = conn.prepare(&query_sql)?;
        let records: Vec<RecordBatch> = stmt.query_arrow([])?.collect();

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

        let wind_direction_arr = record_batch
            .column(7)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 7");

        let humidity_max_arr = record_batch
            .column(8)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 8");

        let humidity_min_arr = record_batch
            .column(9)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 9");

        let temperature_unit_code_arr = record_batch
            .column(10)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 10");

        let precip_chance_arr = record_batch
            .column(11)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 11");

        let rain_amt_arr = record_batch
            .column(12)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 12");

        let snow_amt_arr = record_batch
            .column(13)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 13");

        let ice_amt_arr = record_batch
            .column(14)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 14");

        for row_index in 0..record_batch.num_rows() {
            let station_id = station_id_arr.value(row_index).to_owned();
            let date = date_arr.value(row_index).to_owned();
            let start_time = start_time_arr.value(row_index).to_owned();
            let end_time = end_time_arr.value(row_index).to_owned();
            let temp_low = temp_low_arr.value(row_index);
            let temp_high = temp_high_arr.value(row_index);

            // Check for NULL first, then validate the range
            let wind_speed = if wind_speed_arr.is_null(row_index) {
                None
            } else {
                let wind_speed_val = wind_speed_arr.value(row_index);
                // Filter out unreasonable values (negative or > 500 mph)
                if (0..=500).contains(&wind_speed_val) {
                    Some(wind_speed_val)
                } else {
                    None
                }
            };

            // Wind direction in degrees (0-360)
            let wind_direction = if wind_direction_arr.is_null(row_index) {
                None
            } else {
                let val = wind_direction_arr.value(row_index);
                if (0..=360).contains(&val) {
                    Some(val)
                } else {
                    None
                }
            };

            // Humidity max (0-100%)
            let humidity_max = if humidity_max_arr.is_null(row_index) {
                None
            } else {
                let val = humidity_max_arr.value(row_index);
                if (0..=100).contains(&val) {
                    Some(val)
                } else {
                    None
                }
            };

            // Humidity min (0-100%)
            let humidity_min = if humidity_min_arr.is_null(row_index) {
                None
            } else {
                let val = humidity_min_arr.value(row_index);
                if (0..=100).contains(&val) {
                    Some(val)
                } else {
                    None
                }
            };

            let temp_unit_code = temperature_unit_code_arr.value(row_index).to_owned();

            // Precipitation chance (0-100%)
            let precip_chance = if precip_chance_arr.is_null(row_index) {
                None
            } else {
                let val = precip_chance_arr.value(row_index);
                if (0.0..=100.0).contains(&val) {
                    Some(val.round() as i64)
                } else {
                    None
                }
            };

            // Rain amount in inches
            let rain_amt = if rain_amt_arr.is_null(row_index) {
                None
            } else {
                let val = rain_amt_arr.value(row_index);
                if val >= 0.0 {
                    Some(val)
                } else {
                    None
                }
            };

            // Snow amount in inches
            let snow_amt = if snow_amt_arr.is_null(row_index) {
                None
            } else {
                let val = snow_amt_arr.value(row_index);
                if val >= 0.0 {
                    Some(val)
                } else {
                    None
                }
            };

            // Ice amount in inches
            let ice_amt = if ice_amt_arr.is_null(row_index) {
                None
            } else {
                let val = ice_amt_arr.value(row_index);
                if val >= 0.0 {
                    Some(val)
                } else {
                    None
                }
            };

            let mut forecast = Forecast {
                station_id,
                date,
                start_time,
                end_time,
                temp_low,
                temp_high,
                wind_speed,
                wind_direction,
                humidity_max,
                humidity_min,
                temp_unit_code,
                precip_chance,
                rain_amt,
                snow_amt,
                ice_amt,
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
        // Column order matches the SELECT in observation_data():
        // 0: station_id, 1: start_time, 2: end_time, 3: temp_low, 4: temp_high,
        // 5: wind_speed, 6: temperature_unit_code, 7: wind_direction, 8: humidity,
        // 9: rain_amt, 10: snow_amt, 11: ice_amt
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
        let wind_direction_arr = record_batch
            .column(7)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 7");
        let humidity_arr = record_batch
            .column(8)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 8");
        let rain_amt_arr = record_batch
            .column(9)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 9");
        let snow_amt_arr = record_batch
            .column(10)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 10");
        let ice_amt_arr = record_batch
            .column(11)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 11");

        for row_index in 0..record_batch.num_rows() {
            let station_id = station_id_arr.value(row_index).to_owned();
            let start_time = start_time_arr.value(row_index).to_owned();
            let end_time = end_time_arr.value(row_index).to_owned();
            let temp_low = temp_low_arr.value(row_index);
            let temp_high = temp_high_arr.value(row_index);
            let wind_speed = if wind_speed_arr.is_null(row_index) {
                0
            } else {
                wind_speed_arr.value(row_index)
            };
            let temp_unit_code = temperature_unit_code_arr.value(row_index).to_owned();

            let wind_direction = if wind_direction_arr.is_null(row_index) {
                None
            } else {
                let val = wind_direction_arr.value(row_index);
                if (0..=360).contains(&val) {
                    Some(val)
                } else {
                    None
                }
            };

            let humidity = if humidity_arr.is_null(row_index) {
                None
            } else {
                let val = humidity_arr.value(row_index);
                if (0..=100).contains(&val) {
                    Some(val)
                } else {
                    None
                }
            };

            let rain_amt = if rain_amt_arr.is_null(row_index) {
                None
            } else {
                let val = rain_amt_arr.value(row_index);
                if val >= 0.0 {
                    Some(val)
                } else {
                    None
                }
            };

            let snow_amt = if snow_amt_arr.is_null(row_index) {
                None
            } else {
                let val = snow_amt_arr.value(row_index);
                if val >= 0.0 {
                    Some(val)
                } else {
                    None
                }
            };

            let ice_amt = if ice_amt_arr.is_null(row_index) {
                None
            } else {
                let val = ice_amt_arr.value(row_index);
                if val >= 0.0 {
                    Some(val)
                } else {
                    None
                }
            };

            let mut observation = Observation {
                station_id,
                start_time,
                end_time,
                temp_low,
                temp_high,
                wind_speed,
                temp_unit_code,
                wind_direction,
                humidity,
                rain_amt,
                snow_amt,
                ice_amt,
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
                self.temp_low = self.temp_low * 9.0 / 5.0 + 32.0;
                self.temp_high = self.temp_high * 9.0 / 5.0 + 32.0;
                self.temp_unit_code = target_unit.to_string();
            }
            ("fahrenheit", TemperatureUnit::Celsius) => {
                self.temp_low = (self.temp_low - 32.0) * 5.0 / 9.0;
                self.temp_high = (self.temp_high - 32.0) * 5.0 / 9.0;
                self.temp_unit_code = target_unit.to_string();
            }
            _ => (), // No conversion needed or unknown unit
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
    pub wind_speed: i64,
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

impl DailyObservation {
    pub fn convert_temperature(&mut self, target_unit: &TemperatureUnit) {
        let current_unit = match self.temp_unit_code.to_lowercase().as_str() {
            "celcius" => "celsius".to_string(),
            _ => self.temp_unit_code.to_lowercase(),
        };

        if current_unit == target_unit.to_string() {
            return;
        }

        match (current_unit.as_str(), target_unit) {
            ("celsius", TemperatureUnit::Fahrenheit) => {
                self.temp_low = self.temp_low * 9.0 / 5.0 + 32.0;
                self.temp_high = self.temp_high * 9.0 / 5.0 + 32.0;
                self.temp_unit_code = target_unit.to_string();
            }
            ("fahrenheit", TemperatureUnit::Celsius) => {
                self.temp_low = (self.temp_low - 32.0) * 5.0 / 9.0;
                self.temp_high = (self.temp_high - 32.0) * 5.0 / 9.0;
                self.temp_unit_code = target_unit.to_string();
            }
            _ => (),
        }
    }
}

struct DailyObservations {
    values: Vec<DailyObservation>,
}

impl DailyObservations {
    pub fn new() -> Self {
        DailyObservations { values: Vec::new() }
    }

    pub fn merge(&mut self, observations: DailyObservations) -> &DailyObservations {
        self.values.extend(observations.values);
        self
    }

    pub fn from_with_temp_unit(record_batch: &RecordBatch, target_unit: &TemperatureUnit) -> Self {
        let mut observations = Vec::new();
        // Column order matches the SELECT in daily_observations():
        // 0: station_id, 1: date, 2: temp_low, 3: temp_high, 4: wind_speed,
        // 5: temperature_unit_code, 6: wind_direction, 7: humidity,
        // 8: rain_amt, 9: snow_amt, 10: ice_amt
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
        let temp_low_arr = record_batch
            .column(2)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 2");
        let temp_high_arr = record_batch
            .column(3)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 3");
        let wind_speed_arr = record_batch
            .column(4)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 4");
        let temperature_unit_code_arr = record_batch
            .column(5)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 5");
        let wind_direction_arr = record_batch
            .column(6)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 6");
        let humidity_arr = record_batch
            .column(7)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Expected Int64Array in column 7");
        let rain_amt_arr = record_batch
            .column(8)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 8");
        let snow_amt_arr = record_batch
            .column(9)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 9");
        let ice_amt_arr = record_batch
            .column(10)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 10");

        for row_index in 0..record_batch.num_rows() {
            let station_id = station_id_arr.value(row_index).to_owned();
            let date = date_arr.value(row_index).to_owned();
            let temp_low = temp_low_arr.value(row_index);
            let temp_high = temp_high_arr.value(row_index);
            let wind_speed = if wind_speed_arr.is_null(row_index) {
                0
            } else {
                wind_speed_arr.value(row_index)
            };
            let temp_unit_code = temperature_unit_code_arr.value(row_index).to_owned();

            let wind_direction = if wind_direction_arr.is_null(row_index) {
                None
            } else {
                let val = wind_direction_arr.value(row_index);
                if (0..=360).contains(&val) {
                    Some(val)
                } else {
                    None
                }
            };

            let humidity = if humidity_arr.is_null(row_index) {
                None
            } else {
                let val = humidity_arr.value(row_index);
                if (0..=100).contains(&val) {
                    Some(val)
                } else {
                    None
                }
            };

            let rain_amt = if rain_amt_arr.is_null(row_index) {
                None
            } else {
                let val = rain_amt_arr.value(row_index);
                if val >= 0.0 {
                    Some(val)
                } else {
                    None
                }
            };

            let snow_amt = if snow_amt_arr.is_null(row_index) {
                None
            } else {
                let val = snow_amt_arr.value(row_index);
                if val >= 0.0 {
                    Some(val)
                } else {
                    None
                }
            };

            let ice_amt = if ice_amt_arr.is_null(row_index) {
                None
            } else {
                let val = ice_amt_arr.value(row_index);
                if val >= 0.0 {
                    Some(val)
                } else {
                    None
                }
            };

            let mut observation = DailyObservation {
                station_id,
                date,
                temp_low,
                temp_high,
                wind_speed,
                temp_unit_code,
                wind_direction,
                humidity,
                rain_amt,
                snow_amt,
                ice_amt,
            };
            observation.convert_temperature(target_unit);
            observations.push(observation);
        }

        Self {
            values: observations,
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
        let state_arr = record_batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 2");
        let iata_id_arr = record_batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Expected StringArray in column 3");
        let elevation_m_arr = record_batch
            .column(4)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 4");
        let latitude_arr = record_batch
            .column(5)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 5");
        let longitude_arr = record_batch
            .column(6)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("Expected Float64Array in column 6");

        for row_index in 0..record_batch.num_rows() {
            let station_id = station_id_arr.value(row_index).to_owned();
            let station_name = station_name_arr.value(row_index).to_owned();
            let state = state_arr.value(row_index).to_owned();
            let iata_id = iata_id_arr.value(row_index).to_owned();
            let elevation_m = if elevation_m_arr.is_null(row_index) {
                None
            } else {
                Some(elevation_m_arr.value(row_index))
            };
            let latitude = latitude_arr.value(row_index);
            let longitude = longitude_arr.value(row_index);

            stations.push(Station {
                station_id,
                station_name,
                state,
                iata_id,
                elevation_m,
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
    pub state: String,
    pub iata_id: String,
    pub elevation_m: Option<f64>,
    pub latitude: f64,
    pub longitude: f64,
}
