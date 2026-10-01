-- Exploratory daily report extrema in UTC, with temperatures in Fahrenheit.
-- Keep the newest publication of each station/report instant before aggregation.
-- Raw METAR precipitation reports can overlap and cannot establish a daily
-- phase total. Show the largest reported hourly liquid amount, never a sum or
-- a snow-depth estimate. Production settlement also validates source evidence.
WITH deduped_observations AS (
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
)
SELECT
    station_id,
    COUNT(*) AS distinct_reports,
    DATE_TRUNC('day', generated_at::TIMESTAMPTZ AT TIME ZONE 'UTC')::TEXT AS date_utc,
    MIN(temperature_f) FILTER (WHERE isfinite(temperature_f)) AS temp_low_f,
    MAX(temperature_f) FILTER (WHERE isfinite(temperature_f)) AS temp_high_f,
    MAX(wind_speed) FILTER (WHERE wind_speed BETWEEN 0 AND 500) AS wind_speed_knots,
    FIRST(wind_direction ORDER BY wind_speed DESC, generated_at::TIMESTAMPTZ DESC)
        FILTER (WHERE wind_speed BETWEEN 0 AND 500 AND wind_direction BETWEEN 0 AND 360)
        AS direction_at_peak_wind_degrees,
    MAX(precip_in) FILTER (
        WHERE isfinite(precip_in) AND precip_in >= 0
          AND json_extract_string(to_json(normalized_observations), '$.metar_type') = 'METAR'
    ) AS max_reported_hourly_liquid_precip_in
FROM normalized_observations
GROUP BY station_id, date_utc
ORDER BY station_id, date_utc
