# Retained RR7 regression evidence

`rr7-mci-positive.json` is the unchanged [official KMCI RR7 product JSON](https://api.weather.gov/products/96ec7fd5-9bc1-434e-b77d-25e8754abc79), retrieved on 2026-09-27.
Its PPH amount is 0.21 inches for 2026-09-26 07:00–08:00 UTC.

`stations-20260927.xml.gz` retains the complete [official AWC station catalog](https://aviationweather.gov/data/cache/stations.cache.xml.gz), retrieved at approximately 14:47 UTC on 2026-09-27.
The fixture recompresses the original decoded XML with a deterministic gzip timestamp.
The SHA-256 of the decoded original bytes is `8267824d92063561f7fbfccb507ab9ee4cdeed98240be0298bde595adc1bccc8`.
The catalog explicitly maps IATA `MCI` to station `KMCI`.

`rr7-nyc-positive.json` is an unchanged [official NYC RR7 product JSON](https://api.weather.gov/products/458b17e0-d6d0-401a-9ddd-436d4fb0b137).
The small NYC mapping in unit tests is synthetic.
The retained live catalog has no NYC alias, so production cannot map that bulletin to KNYC.

Receipt times and METAR rows in reader tests isolate verification and interval behavior.
They do not claim a live settlement result for those historical windows.
