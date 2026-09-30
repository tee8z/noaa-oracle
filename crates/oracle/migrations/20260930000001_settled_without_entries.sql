-- An event that reaches its signing date without entries has no outcome to
-- sign. Recording when the oracle found that lets processing leave the event
-- alone instead of reading it every pass until its expiry. The attestation
-- stays null, and existing events are settled by the next pass.
ALTER TABLE events ADD COLUMN settled_without_entries_at INTEGER;
