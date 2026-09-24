//! The forecast query as it was before derived forecast files, kept as the
//! reference the current query must agree with.

use super::{
    Error, FileParams, Forecast, PUBLICATION_GRACE, WeatherAccess, decode_forecasts,
    empty_publication_window, forecast_generated_window, sql_string_list, station_filter,
    utc_timestamp_sql,
};
use crate::routes::ForecastRequest;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

pub(super) async fn forecasts_data(
    access: &WeatherAccess,
    req: &ForecastRequest,
    station_ids: Vec<String>,
) -> Result<Vec<Forecast>, Error> {
    let station_filter = station_filter(&station_ids)?;
    let now = OffsetDateTime::now_utc();
    let (generated_start, generated_end) = forecast_generated_window(req, now);
    // Files are named by publication time, not forecast validity. A
    // qualifying issue can be published after the generation cutoff.
    let file_params = FileParams {
        start: Some(generated_start.to_offset(UtcOffset::UTC)),
        end: Some(
            generated_end
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
    let parquet_files = access.file_access.grab_file_names(file_params).await?;
    let file_paths = access.file_access.build_file_paths(parquet_files);
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

    time_filters.push(format!(
        "generated_at::TIMESTAMPTZ >= '{}'::TIMESTAMPTZ",
        generated_start.format(&Rfc3339)?
    ));
    time_filters.push(format!(
        "generated_at::TIMESTAMPTZ <= '{}'::TIMESTAMPTZ",
        generated_end.format(&Rfc3339)?
    ));

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
            "GREATEST('{}'::TIMESTAMPTZ, MIN(df.start_time))",
            start.format(&Rfc3339)?
        )
    } else {
        "MIN(df.start_time)".to_string()
    };
    let end_time_expr = if let Some(end) = &req.end {
        format!(
            "LEAST('{}'::TIMESTAMPTZ, MAX(df.end_time))",
            end.format(&Rfc3339)?
        )
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
                       NULL::VARCHAR AS generated_at, NULL::VARCHAR AS filename
                WHERE false
                UNION ALL BY NAME
                SELECT * FROM read_parquet([{}], union_by_name = true, filename = true)
            )
        ),
        normalized_forecasts AS (
            SELECT * EXCLUDE (min_temp, max_temp, temperature_unit_code),
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
                'fahrenheit'::VARCHAR AS temperature_unit_code
            FROM parquet_data
        ),
        -- Deduplicate each station/window by issue instant, then newest
        -- publication of that issue. Value ties are deterministic.
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
            FROM normalized_forecasts
            {} {}
            ORDER BY station_id, begin_time::TIMESTAMPTZ, end_time::TIMESTAMPTZ, generated_at::TIMESTAMPTZ DESC,
                     regexp_extract(filename, '(^|/)forecasts_([^/]+)\.parquet$', 2)::TIMESTAMPTZ DESC,
                     filename DESC, max_temp DESC, min_temp DESC, wind_speed DESC, wind_direction DESC,
                     relative_humidity_max DESC, relative_humidity_min DESC,
                     twelve_hour_probability_of_precipitation DESC, liquid_precipitation_amt DESC,
                     snow_amt DESC, snow_ratio DESC, ice_amt DESC
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
                SUM(CASE WHEN pr.snow_amt = 0 THEN 0 ELSE pr.snow_amt / pr.snow_ratio END)
                    FILTER (WHERE pr.snow_amt IS NOT NULL AND pr.snow_amt >= 0 AND pr.snow_ratio > 0) AS snow_liquid_amt
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
                s.snow_liquid_amt,
                i.ice_amt
            FROM daily_qpf q
            FULL OUTER JOIN daily_snow s ON q.station_id = s.station_id AND q.date = s.date
            FULL OUTER JOIN daily_ice i ON COALESCE(q.station_id, s.station_id) = i.station_id AND COALESCE(q.date, s.date) = i.date
        ),
        daily_forecasts AS (
            SELECT
                station_id,
                DATE_TRUNC('day', begin_time::TIMESTAMPTZ AT TIME ZONE 'UTC')::TEXT AS date,
                MIN(begin_time::TIMESTAMPTZ) AS start_time,
                MAX(end_time::TIMESTAMPTZ) AS end_time,
                MIN(min_temp) FILTER (WHERE min_temp IS NOT NULL AND min_temp >= -200 AND min_temp <= 200) AS temp_low,
                MAX(max_temp) FILTER (WHERE max_temp IS NOT NULL AND max_temp >= -200 AND max_temp <= 200) AS temp_high,
                MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500) AS wind_speed,
                -- Direction belongs to the strongest wind period; a missing
                -- direction there must not fall back to a weaker period.
                FIRST(wind_direction ORDER BY wind_speed DESC, begin_time::TIMESTAMPTZ DESC, end_time::TIMESTAMPTZ DESC)
                    FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500) AS wind_direction,
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
            CASE WHEN dp.total_qpf IS NULL THEN NULL ELSE GREATEST(0,
                dp.total_qpf - COALESCE(dp.snow_liquid_amt, 0) - COALESCE(dp.ice_amt, 0)
            ) END::DOUBLE AS rain_amt,
            dp.snow_amt::DOUBLE AS snow_amt,
            dp.ice_amt::DOUBLE AS ice_amt
        FROM daily_forecasts df
        LEFT JOIN daily_precip dp ON df.station_id = dp.station_id AND df.date = dp.date
        GROUP BY df.station_id, df.date, dp.total_qpf, dp.snow_amt, dp.snow_liquid_amt, dp.ice_amt
        "#,
        sql_string_list(&file_paths),
        station_filter,
        time_filter,
        utc_timestamp_sql(&start_time_expr),
        utc_timestamp_sql(&end_time_expr),
    );

    let unit = req.temperature_unit;
    access
        .query(query_sql, move |batches| decode_forecasts(batches, &unit))
        .await
}
