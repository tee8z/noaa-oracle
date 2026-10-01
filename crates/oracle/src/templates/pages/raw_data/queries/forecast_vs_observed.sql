-- Exploratory comparison in Fahrenheit; differences are Fahrenheit degrees.
-- Forecast extrema group native intervals by their UTC start day. They are
-- not strict settlement baselines: intervals may extend beyond that day.
-- Deduplicate report snapshots and normalize each source before aggregation.
WITH deduped_forecasts AS (
    SELECT * FROM forecasts
    QUALIFY ROW_NUMBER() OVER (
        PARTITION BY station_id, begin_time::TIMESTAMPTZ, end_time::TIMESTAMPTZ
        ORDER BY generated_at::TIMESTAMPTZ DESC,
                 regexp_extract(filename, 'forecasts_([^/]+)\.parquet$', 1)::TIMESTAMPTZ DESC,
                 filename DESC, min_temp DESC, max_temp DESC
    ) = 1
),
normalized_forecasts AS (
    SELECT *,
        CASE lower(temperature_unit_code)
            WHEN 'fahrenheit' THEN min_temp
            WHEN 'celsius' THEN min_temp * 9.0 / 5.0 + 32.0
            WHEN 'celcius' THEN min_temp * 9.0 / 5.0 + 32.0
        END AS min_temp_f,
        CASE lower(temperature_unit_code)
            WHEN 'fahrenheit' THEN max_temp
            WHEN 'celsius' THEN max_temp * 9.0 / 5.0 + 32.0
            WHEN 'celcius' THEN max_temp * 9.0 / 5.0 + 32.0
        END AS max_temp_f
    FROM deduped_forecasts
),
deduped_observations AS (
    SELECT * FROM observations
    QUALIFY ROW_NUMBER() OVER (
        PARTITION BY station_id, generated_at::TIMESTAMPTZ
        ORDER BY regexp_extract(filename, 'observations_([^/]+)\.parquet$', 1)::TIMESTAMPTZ DESC,
                 filename DESC, temperature_value DESC, wind_speed DESC,
                 wind_direction DESC, dewpoint_value DESC, precip_in DESC
    ) = 1
),
normalized_observations AS (
    SELECT *,
        CASE lower(temperature_unit_code)
            WHEN 'fahrenheit' THEN temperature_value
            WHEN 'celsius' THEN temperature_value * 9.0 / 5.0 + 32.0
            WHEN 'celcius' THEN temperature_value * 9.0 / 5.0 + 32.0
        END AS temperature_f
    FROM deduped_observations
),
daily_fcst AS (
    SELECT station_id,
        DATE_TRUNC('day', begin_time::TIMESTAMPTZ AT TIME ZONE 'UTC')::TEXT AS date_utc,
        MIN(min_temp_f) FILTER (WHERE isfinite(min_temp_f)) AS temp_low_f,
        MAX(max_temp_f) FILTER (WHERE isfinite(max_temp_f)) AS temp_high_f
    FROM normalized_forecasts
    GROUP BY station_id, date_utc
),
daily_obs AS (
    SELECT station_id,
        DATE_TRUNC('day', generated_at::TIMESTAMPTZ AT TIME ZONE 'UTC')::TEXT AS date_utc,
        MIN(temperature_f) FILTER (WHERE isfinite(temperature_f)) AS temp_low_f,
        MAX(temperature_f) FILTER (WHERE isfinite(temperature_f)) AS temp_high_f
    FROM normalized_observations
    GROUP BY station_id, date_utc
)
SELECT f.station_id, f.date_utc,
    f.temp_high_f AS forecast_high_f, f.temp_low_f AS forecast_low_f,
    o.temp_high_f AS observed_high_f, o.temp_low_f AS observed_low_f,
    o.temp_high_f - f.temp_high_f AS high_difference_f,
    o.temp_low_f - f.temp_low_f AS low_difference_f
FROM daily_fcst f
JOIN daily_obs o ON f.station_id = o.station_id AND f.date_utc = o.date_utc
ORDER BY f.station_id, f.date_utc
