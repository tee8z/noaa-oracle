-- Station list: all unique stations with metadata
SELECT DISTINCT
    station_id,
    COALESCE(station_name, '') AS station_name,
    COALESCE(state, '') AS state,
    COALESCE(iata_id, '') AS iata_id,
    elevation_m, latitude, longitude
FROM observations
ORDER BY state, station_id
