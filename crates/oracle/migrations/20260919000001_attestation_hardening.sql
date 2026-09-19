-- Events created before this migration stored their secret signing nonce
-- and served it over the API. Attesting any of them would reveal the oracle
-- key (s = k + e*d with k public), so they are discarded rather than kept
-- unsignable. No event had been attested when this migration was written.
--
-- The new schema stores only public nonce material (a salt and the nonce
-- point); the nonce itself is derived from the key at signing time. Entry
-- choices and readings are source independent: one row per
-- (target, metric).

DROP TABLE IF EXISTS events_weather;
DROP TABLE IF EXISTS weather;
DROP TABLE IF EXISTS expected_observations;
DROP TABLE IF EXISTS events_entries;
DROP TABLE IF EXISTS events;

CREATE TABLE events (
    id TEXT PRIMARY KEY NOT NULL,
    source TEXT NOT NULL,
    total_allowed_entries INTEGER NOT NULL CHECK (total_allowed_entries > 1),
    number_of_places_win INTEGER NOT NULL
        CHECK (number_of_places_win > 0 AND number_of_places_win < total_allowed_entries),
    number_of_values_per_entry INTEGER NOT NULL CHECK (number_of_values_per_entry > 0),
    signing_date INTEGER NOT NULL,
    start_observation_date INTEGER NOT NULL,
    end_observation_date INTEGER NOT NULL,
    nonce_salt BLOB NOT NULL CHECK (length(nonce_salt) = 32),
    nonce_point BLOB NOT NULL CHECK (length(nonce_point) = 33),
    event_announcement BLOB NOT NULL,
    locations TEXT NOT NULL,
    metrics TEXT NOT NULL,
    coordinator_pubkey TEXT NOT NULL,
    attestation BLOB CHECK (attestation IS NULL OR length(attestation) = 32),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK (start_observation_date < end_observation_date),
    CHECK (end_observation_date <= signing_date)
) STRICT;

CREATE INDEX idx_events_unattested ON events(id) WHERE attestation IS NULL;

CREATE TABLE events_entries (
    id TEXT PRIMARY KEY NOT NULL,
    event_id TEXT NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    score INTEGER,
    base_score INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE INDEX idx_events_entries_event_id ON events_entries(event_id);

CREATE TABLE entry_picks (
    entry_id TEXT NOT NULL REFERENCES events_entries(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    target TEXT NOT NULL,
    metric TEXT NOT NULL,
    prediction TEXT NOT NULL CHECK (prediction IN ('over', 'par', 'under')),
    PRIMARY KEY (entry_id, target, metric),
    UNIQUE (entry_id, position)
) STRICT;

CREATE TABLE event_readings (
    event_id TEXT NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    target TEXT NOT NULL,
    metric TEXT NOT NULL,
    baseline REAL,
    observed REAL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (event_id, target, metric)
) STRICT;
