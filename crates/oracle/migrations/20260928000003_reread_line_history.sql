-- 2.5.0 read every retained window again after its reset, but forecasts from
-- before native interval provenance gave no whole-period baseline, so each
-- window was stored with no pairs and lines were fitted from nothing. The
-- line pass now falls back to those forecasts' daily roll-up; read the
-- windows once more. Events keep the lines they copied.
DELETE FROM line_pairs;
DELETE FROM line_windows;
