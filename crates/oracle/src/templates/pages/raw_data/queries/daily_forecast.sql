-- Exploratory summaries of native forecast intervals by their UTC start day.
-- Temperatures are Fahrenheit. Intervals can overlap or cross midnight, so
-- precipitation columns show interval maxima, not invented daily totals.
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
)
SELECT
    station_id,
    DATE_TRUNC('day', begin_time::TIMESTAMPTZ AT TIME ZONE 'UTC')::TEXT AS date_utc,
    MIN(begin_time::TIMESTAMPTZ) AS first_interval_start_utc,
    MAX(end_time::TIMESTAMPTZ) AS last_interval_end_utc,
    MIN(min_temp_f) FILTER (WHERE isfinite(min_temp_f)) AS temp_low_f,
    MAX(max_temp_f) FILTER (WHERE isfinite(max_temp_f)) AS temp_high_f,
    MAX(wind_speed) FILTER (WHERE wind_speed BETWEEN 0 AND 500) AS wind_speed_knots,
    MAX(relative_humidity_max) FILTER (WHERE relative_humidity_max BETWEEN 0 AND 100) AS humidity_max_pct,
    MIN(relative_humidity_min) FILTER (WHERE relative_humidity_min BETWEEN 0 AND 100) AS humidity_min_pct,
    MAX(twelve_hour_probability_of_precipitation) FILTER (
        WHERE twelve_hour_probability_of_precipitation BETWEEN 0 AND 100
    ) AS precip_chance_pct,
    MAX(liquid_precipitation_amt) FILTER (
        WHERE isfinite(liquid_precipitation_amt) AND liquid_precipitation_amt >= 0
    ) AS max_interval_liquid_precip_in,
    MAX(snow_amt) FILTER (WHERE isfinite(snow_amt) AND snow_amt >= 0) AS max_interval_snow_in,
    MAX(ice_amt) FILTER (WHERE isfinite(ice_amt) AND ice_amt >= 0) AS max_interval_ice_in
FROM normalized_forecasts
GROUP BY station_id, date_utc
ORDER BY station_id, date_utc
