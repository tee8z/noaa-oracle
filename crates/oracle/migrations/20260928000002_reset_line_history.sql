-- Line history read before 2.4.3 compared observations with a daily roll-up
-- of forecasts, not the whole-period baseline events are scored against.
-- Drop it; the line pass reads the retained windows again and its first
-- refit replaces the current lines. Events keep the lines they copied.
DELETE FROM line_pairs;
DELETE FROM line_windows;
