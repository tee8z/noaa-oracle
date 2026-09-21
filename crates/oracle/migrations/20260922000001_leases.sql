-- Work that only one oracle process may do at a time. Two processes share this
-- database during a blue/green deploy; only the lease holder runs processing,
-- which scores entries and signs attestations.
CREATE TABLE IF NOT EXISTS leases (
    name TEXT PRIMARY KEY NOT NULL,
    holder TEXT NOT NULL,
    -- UNIX milliseconds.
    expires_at INTEGER NOT NULL
);
