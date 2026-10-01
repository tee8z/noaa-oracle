CREATE TABLE nip98_consumed_events (event_id TEXT PRIMARY KEY NOT NULL, expires_at INTEGER NOT NULL);
CREATE INDEX nip98_consumed_expiry ON nip98_consumed_events(expires_at);
