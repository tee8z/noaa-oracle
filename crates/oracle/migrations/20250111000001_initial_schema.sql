-- Oracle metadata (singleton table)
CREATE TABLE IF NOT EXISTS oracle_metadata (
    pubkey BLOB NOT NULL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Events table
CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    total_allowed_entries INTEGER NOT NULL,
    number_of_places_win INTEGER NOT NULL,
    number_of_values_per_entry INTEGER NOT NULL,
    signing_date INTEGER NOT NULL,
    start_observation_date INTEGER NOT NULL,
    end_observation_date INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    nonce BLOB NOT NULL,
    event_announcement BLOB NOT NULL,
    locations TEXT NOT NULL,
    coordinator_pubkey TEXT,
    attestation_signature BLOB,
    scoring_fields TEXT
);

CREATE INDEX IF NOT EXISTS idx_events_signing_date ON events(signing_date);
CREATE INDEX IF NOT EXISTS idx_events_attestation ON events(attestation_signature);

-- Event entries table
CREATE TABLE IF NOT EXISTS events_entries (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL REFERENCES events(id),
    score INTEGER NOT NULL DEFAULT 0,
    base_score INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_events_entries_event_id ON events_entries(event_id);

-- Expected observations (entry choices)
CREATE TABLE IF NOT EXISTS expected_observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_id TEXT NOT NULL REFERENCES events_entries(id),
    station TEXT NOT NULL,
    temp_low TEXT CHECK(temp_low IN ('over', 'par', 'under')),
    temp_high TEXT CHECK(temp_high IN ('over', 'par', 'under')),
    wind_speed TEXT CHECK(wind_speed IN ('over', 'par', 'under')),
    wind_direction TEXT CHECK(wind_direction IN ('over', 'par', 'under')),
    rain_amt TEXT CHECK(rain_amt IN ('over', 'par', 'under')),
    snow_amt TEXT CHECK(snow_amt IN ('over', 'par', 'under')),
    humidity TEXT CHECK(humidity IN ('over', 'par', 'under'))
);

CREATE INDEX IF NOT EXISTS idx_expected_observations_entry_id ON expected_observations(entry_id);

-- Weather readings cache
CREATE TABLE IF NOT EXISTS weather (
    id TEXT PRIMARY KEY,
    station_id TEXT NOT NULL,
    observed_date INTEGER,
    observed_temp_low INTEGER,
    observed_temp_high INTEGER,
    observed_wind_speed INTEGER,
    forecasted_date INTEGER NOT NULL,
    forecasted_temp_low INTEGER NOT NULL,
    forecasted_temp_high INTEGER NOT NULL,
    forecasted_wind_speed INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_weather_station_id ON weather(station_id);

-- Events to weather junction table
CREATE TABLE IF NOT EXISTS events_weather (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL REFERENCES events(id),
    weather_id TEXT NOT NULL REFERENCES weather(id),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_events_weather_event_id ON events_weather(event_id);
CREATE INDEX IF NOT EXISTS idx_events_weather_weather_id ON events_weather(weather_id);
