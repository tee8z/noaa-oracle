-- Daily observations: powers the weather map and dashboard
-- Groups hourly observations by station and day, classifies precipitation
-- using METAR weather codes, and derives humidity via the Magnus formula
WITH classified AS (
    SELECT *,
        CASE
            WHEN wx_string IS NOT NULL AND wx_string != '' THEN
                CASE
                    WHEN regexp_matches(wx_string, '(^|\s)[-+]?(VC)?(([A-Z]{2})*(PL|GR|GS|IC)|FZ(RA|DZ))(\s|$|[A-Z])') THEN 'ice'
                    WHEN regexp_matches(wx_string, '(^|\s)[-+]?(VC)?([A-Z]{2})*(SN|SG)(\s|$|[A-Z])') THEN 'snow'
                    ELSE 'rain'
                END
            WHEN temperature_value IS NOT NULL AND temperature_value <= 2.0 THEN 'snow'
            ELSE 'rain'
        END AS precip_type
    FROM observations
)
SELECT
    station_id,
    DATE_TRUNC('day', generated_at::TIMESTAMP)::TEXT AS date,
    MIN(temperature_value) FILTER (WHERE temperature_value IS NOT NULL) AS temp_low,
    MAX(temperature_value) FILTER (WHERE temperature_value IS NOT NULL) AS temp_high,
    MAX(wind_speed) FILTER (WHERE wind_speed IS NOT NULL AND wind_speed >= 0 AND wind_speed <= 500) AS wind_speed,
    MAX(wind_direction) FILTER (WHERE wind_direction IS NOT NULL AND wind_direction >= 0 AND wind_direction <= 360) AS wind_direction,
    MAX(temperature_unit_code) AS temperature_unit_code,
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
ORDER BY station_id, date
