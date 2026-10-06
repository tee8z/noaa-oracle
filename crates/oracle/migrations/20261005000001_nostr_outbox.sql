-- Announcements and attestations waiting to be published to Nostr relays,
-- one row per event, stage and relay (see src/publication.rs). Rows are
-- queued after the event write commits and kept once published, so a
-- restart's backfill does not send them again.
CREATE TABLE IF NOT EXISTS nostr_outbox (
    event_id TEXT NOT NULL,
    -- 'announced' or 'attested'
    stage TEXT NOT NULL CHECK (stage IN ('announced', 'attested')),
    relay TEXT NOT NULL,
    queued_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    -- UNIX seconds; retried with backoff after a failure.
    next_attempt_at INTEGER NOT NULL,
    last_error TEXT,
    published_at INTEGER,
    -- The Nostr event the relay accepted, and its created_at.
    nostr_event_id TEXT,
    nostr_created_at INTEGER,
    PRIMARY KEY (event_id, stage, relay)
);

CREATE INDEX IF NOT EXISTS nostr_outbox_due
    ON nostr_outbox (next_attempt_at) WHERE published_at IS NULL;
