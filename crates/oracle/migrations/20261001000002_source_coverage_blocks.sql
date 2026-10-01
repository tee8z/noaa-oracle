-- Whether an event's latest failed check found the published observations
-- short of its window rather than a fault in the oracle, so monitoring can
-- tell the two apart after a restart. Existing blocks are classified from
-- the message the check stored; the next check of each event rewrites it.
ALTER TABLE event_settlement_blocks
    ADD COLUMN source_coverage INTEGER NOT NULL DEFAULT 0 CHECK (source_coverage IN (0, 1));
UPDATE event_settlement_blocks SET source_coverage = 1
    WHERE message LIKE '%observation coverage is incomplete for %';
