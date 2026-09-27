-- Scoring against self-calibrating lines (see src/lines.rs).
--
-- Existing events keep the fixed Par rules they were created with. An event
-- created with `lines` copies the line for each of its targets and metrics
-- into event_lines when it is created, and is scored against those copies.

ALTER TABLE events ADD COLUMN scoring_rules TEXT NOT NULL DEFAULT 'fixed'
    CHECK (scoring_rules IN ('fixed', 'lines'));

CREATE TABLE event_lines (
    event_id TEXT NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    target TEXT NOT NULL,
    metric TEXT NOT NULL,
    lower REAL NOT NULL,
    upper REAL NOT NULL,
    level TEXT NOT NULL CHECK (level IN ('station', 'pooled')),
    window_hours INTEGER NOT NULL,
    windows INTEGER NOT NULL,
    over INTEGER NOT NULL,
    par INTEGER NOT NULL,
    under INTEGER NOT NULL,
    first_window INTEGER NOT NULL,
    last_window INTEGER NOT NULL,
    fitted_at INTEGER NOT NULL,
    PRIMARY KEY (event_id, target, metric),
    CHECK (lower < upper)
) STRICT;

-- Every window the line history has read, including windows that yielded no
-- pairs, so a pass does not read them again.
CREATE TABLE line_windows (
    source TEXT NOT NULL,
    window_hours INTEGER NOT NULL,
    window_start INTEGER NOT NULL,
    pairs INTEGER NOT NULL,
    collected_at INTEGER NOT NULL,
    PRIMARY KEY (source, window_hours, window_start)
) STRICT;

-- Forecast and observation per target, metric, and past window: the history
-- lines are fitted on. Older rows are pruned.
CREATE TABLE line_pairs (
    source TEXT NOT NULL,
    window_hours INTEGER NOT NULL,
    metric TEXT NOT NULL,
    target TEXT NOT NULL,
    window_start INTEGER NOT NULL,
    baseline REAL NOT NULL,
    observed REAL NOT NULL,
    PRIMARY KEY (source, window_hours, metric, target, window_start)
) STRICT, WITHOUT ROWID;

-- The latest fit. `target` is empty for the pooled line of a metric, which
-- targets without enough history of their own use.
CREATE TABLE line_fits (
    source TEXT NOT NULL,
    window_hours INTEGER NOT NULL,
    metric TEXT NOT NULL,
    target TEXT NOT NULL,
    lower REAL NOT NULL,
    upper REAL NOT NULL,
    windows INTEGER NOT NULL,
    over INTEGER NOT NULL,
    par INTEGER NOT NULL,
    under INTEGER NOT NULL,
    first_window INTEGER NOT NULL,
    last_window INTEGER NOT NULL,
    fitted_at INTEGER NOT NULL,
    PRIMARY KEY (source, window_hours, metric, target),
    CHECK (lower < upper)
) STRICT, WITHOUT ROWID;
