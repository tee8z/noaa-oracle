-- Daily forecast summary: powers the forecast detail page
-- Deduplicates overlapping forecast windows (keeps latest generated_at),
-- then aggregates to daily granularity with rain/snow/ice separation
WITH deduped_forecasts AS (
    SELECT DISTINCT ON (station_id, begin_time, end_time)
        station_id, begin_time, end_time, min_temp, max_temp,
        wind_speed, wind_direction, relative_humidity_max, relative_humidity_min,
        temperature_unit_code, twelve_hour_probability_of_precipitation,
        liquid_precipitation_amt, snow_amt, snow_ratio, ice_amt, generated_at
    FROM forecasts
    ORDER BY station_id, begin_time, end_time, generated_at DESC
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
        MAX(wind_direction) FILTER (WHERE wind_direction IS NOT NULL AND wind_direction >= 0 AND wind_direction <= 360) AS wind_direction,
        MAX(relative_humidity_max) FILTER (WHERE relative_humidity_max IS NOT NULL AND relative_humidity_max >= 0 AND relative_humidity_max <= 100) AS humidity_max,
        MIN(relative_humidity_min) FILTER (WHERE relative_humidity_min IS NOT NULL AND relative_humidity_min >= 0 AND relative_humidity_min <= 100) AS humidity_min,
        MAX(temperature_unit_code) AS temperature_unit_code,
        MAX(twelve_hour_probability_of_precipitation) FILTER (WHERE twelve_hour_probability_of_precipitation IS NOT NULL) AS precip_chance,
        SUM(liquid_precipitation_amt) FILTER (WHERE liquid_precipitation_amt IS NOT NULL AND liquid_precipitation_amt >= 0) AS total_qpf,
        SUM(snow_amt) FILTER (WHERE snow_amt IS NOT NULL AND snow_amt >= 0) AS snow_amt,
        AVG(snow_ratio) FILTER (WHERE snow_ratio IS NOT NULL AND snow_ratio > 0) AS avg_snow_ratio,
        SUM(ice_amt) FILTER (WHERE ice_amt IS NOT NULL AND ice_amt >= 0) AS ice_amt
    FROM deduped_forecasts
    GROUP BY station_id, DATE_TRUNC('day', begin_time::TIMESTAMP)::TEXT
)
SELECT
    station_id, date, MIN(start_time) AS start_time, MAX(end_time) AS end_time,
    MIN(temp_low) AS temp_low, MAX(temp_high) AS temp_high,
    MAX(wind_speed) AS wind_speed, MAX(wind_direction) AS wind_direction,
    MAX(humidity_max) AS humidity_max, MIN(humidity_min) AS humidity_min,
    MAX(temperature_unit_code) AS temperature_unit_code,
    MAX(precip_chance) AS precip_chance,
    GREATEST(0, COALESCE(
        SUM(total_qpf) - (SUM(snow_amt) / NULLIF(AVG(avg_snow_ratio), 0)) - COALESCE(SUM(ice_amt), 0),
        SUM(total_qpf) - COALESCE(SUM(ice_amt), 0)
    )) AS rain_amt,
    SUM(snow_amt) AS snow_amt,
    SUM(ice_amt) AS ice_amt
FROM daily_forecasts
GROUP BY station_id, date
ORDER BY station_id, date
