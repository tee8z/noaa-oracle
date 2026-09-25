-- A coordinator can create an event unlisted: the events page and the
-- dashboard counts leave it out unless the reader asks for unlisted events,
-- while its own page and the API still serve it. Existing events stay listed.
ALTER TABLE events ADD COLUMN unlisted INTEGER NOT NULL DEFAULT 0 CHECK (unlisted IN (0, 1));
