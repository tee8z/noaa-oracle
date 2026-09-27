-- Retryable failures remain visible after a process restart. The signed event
-- record and existing attestation contract are unchanged.
CREATE TABLE event_settlement_blocks (
    event_id TEXT PRIMARY KEY NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    code TEXT NOT NULL,
    message TEXT NOT NULL,
    checked_at INTEGER NOT NULL
) STRICT;
