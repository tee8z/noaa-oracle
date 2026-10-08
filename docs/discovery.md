# Weather discovery

`GET /stations/eligible/forecasts` returns eligible station metadata and display
forecasts in one response. It uses the same eligibility and forecast-quality checks
as the individual station APIs, with one forecast query for the selected stations.

Supply `start` and `end` as RFC 3339 timestamps. The start must be in the future and
within seven days. The window must contain 1 to 48 whole hours. Optional `days`
sets observation history from 1 to 3 days and defaults to 3; the private listener,
which serves operators, accepts 1 to 31. Eligibility checks use the window length,
capped at 24 hours, and require forecast extent through `end`. Temperatures use
Fahrenheit.

Answers are kept per history and window, the same instants written with any offset
sharing one, until new forecasts arrive or eligibility is judged again, and for at
most 10 minutes. Meanwhile repeated questions are answered at once. After new data a
kept answer is served while one rebuild runs, but not once it is an hour old.

The response has `stations` and `forecasts` arrays. Stations that do not meet
eligibility or forecast extent are excluded. Empty eligibility returns empty arrays
without scanning forecasts. Missing forecast fields remain null. An eligibility or
forecast query failure returns an error instead of a partial discovery response.

The endpoint accepts at most 5,000 server-selected stations. It does not accept
arbitrary station IDs. These display forecasts do not authorize settlement or
guarantee future observations.

## Eligibility

`GET /stations/eligible` and this endpoint judge the same lists. After each
collection run, and a minute past each UTC midnight, the oracle reads the last 3
days of reports once and judges the default list (3 days, 24-hour windows) and every
cached list of up to 3 days from them. Any other history of up to 3 days and any
window length is judged from those reports in milliseconds. These are the only
histories the public listener accepts (`days` 1 to 3; more gets `400`). Longer
histories, up to 31 days, read weeks of report files, alone and in a database of
their own with a smaller memory limit, and only the private listener, which serves
operators, judges them. A cached list is judged again in the background
after the next read or after 10 minutes, and requests get the previous list meanwhile.

The default history must pass on all three days. Histories under ten days permit
no failed days. Longer histories permit one failed day per ten days, rounded down.
Widespread collection outages count as missing evidence; they do not reduce
`days_checked`.

The latest rolling window must also pass settlement's report sampling rule.
This check includes today's reports and windows that cross midnight. It checks
both all usable reports and the subset with usable temperatures. One fresh report
after an outage cannot hide a gap inside that window.

Reports must be newer than 90 minutes. Each station includes `coverage_checked_at`,
`recent_window_hours`, and `max_report_gap_seconds`. The last field includes the
rolling window's edges and measures usable temperature-report gaps. Clients can
prefer stations that do not need settlement's missed-report allowance.

Both endpoints remove entries whose coverage judgment reaches 20 minutes old,
even if a background refresh fails. They also remove entries with expired reports
or forecast extent. Discovery removes the corresponding forecasts from its response.
These checks protect selection; settlement still checks the event's actual data.

`oracle_eligible_stations` counts the default list and changes only when it is
judged, not with request parameters.

## Admission

At most 8 weather requests run at once. Up to 128 more wait up to 10 seconds for a
turn, and each runs for at most 30 seconds. A request beyond that, one that waits too
long, or one that runs too long gets `503` with `Retry-After: 5`, and
`oracle_weather_requests_turned_away_total` counts it. Eligible lists, discovery and
window planning (`/stations/eligible`, `/stations/eligible/forecasts`,
`/stations/window-compatibility`) have a line of their own: 8 at once and 32 waiting,
so a crowd asking for them never holds the turns other weather requests use.

## Heavy work

Judging an eligible list, building a discovery answer and assessing a window read
every station or weeks of files. A few such requests at once could take the oracle
to its memory ceiling while it signs attestations, so this work takes turns from one
budget shared by both listeners and the oracle's own passes:

- At most 2 pieces of heavy work run at once. A build that finds no turn waits up to
  10 seconds for one, behind at most 4 others. When the line is full, or the wait
  runs out, its requests get `503` with `Retry-After: 5`.
- Readers asking the same question at the same time share one build. A build runs in
  a task of its own: a reader who stops waiting after 20 seconds gets `503`, and the
  answer is kept for the next request.
- Histories longer than 3 days run alone.
- A processing pass (scoring and attestation), the preparation of new forecast files
  and the reading of eligibility reports after each collection run take every turn, so nothing heavy runs
  beside them. Each waits for the heavy work already running, and work asked for
  later waits behind it. A pass that has waited 2 minutes runs anyway. Cache warming
  takes a turn per value, so a processing pass waits for at most the values being
  built. Memory freed by heavy work is returned to the system before each pass,
  between the events a pass reads, and after each build.

Kept answers are served whatever the budget. `oracle_heavy_requests_turned_away_total`
counts requests answered `503` for want of a turn or time, and `oracle_cache_entries`
and `oracle_cache_bytes` report the kept answers as `discovery` and
`window_compatibility`.

Available from Oracle v2.7.0. Deploy this release before Coordinator v2.17.1.
