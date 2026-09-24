-- Forecast vs Observed: compares forecast accuracy by joining
-- daily forecast aggregates with daily observation aggregates
WITH deduped_forecasts AS (
    SELECT DISTINCT ON (station_id, begin_time, end_time)
        station_id, begin_time, end_time, min_temp, max_temp, generated_at
    FROM forecasts
    ORDER BY station_id, begin_time, end_time, generated_at DESC
),
daily_fcst AS (
    SELECT
        station_id,
        DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT AS date,
        MIN(min_temp) FILTER (WHERE min_temp >= -200 AND min_temp <= 200) AS temp_low,
        MAX(max_temp) FILTER (WHERE max_temp >= -200 AND max_temp <= 200) AS temp_high
    FROM deduped_forecasts
    GROUP BY station_id, DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT
),
daily_obs AS (
    SELECT
        station_id,
        DATE_TRUNC('day', generated_at::TIMESTAMP)::TEXT AS date,
        MIN(temperature_value) FILTER (WHERE temperature_value IS NOT NULL) AS temp_low,
        MAX(temperature_value) FILTER (WHERE temperature_value IS NOT NULL) AS temp_high
    FROM observations
    GROUP BY station_id, DATE_TRUNC('day', generated_at::TIMESTAMP)::TEXT
)
SELECT
    f.station_id, f.date,
    f.temp_high AS forecast_high, f.temp_low AS forecast_low,
    o.temp_high AS observed_high, o.temp_low AS observed_low,
    f.temp_high - o.temp_high AS high_error,
    f.temp_low - o.temp_low AS low_error
FROM daily_fcst f
JOIN daily_obs o ON f.station_id = o.station_id AND f.date = o.date
ORDER BY f.station_id, f.date
