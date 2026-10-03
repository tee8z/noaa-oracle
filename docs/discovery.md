# Weather discovery

`GET /stations/eligible/forecasts` returns eligible station metadata and display
forecasts in one response. It uses the same eligibility and forecast-quality checks
as the individual station APIs, with one forecast query for the selected stations.

Supply `start` and `end` as RFC 3339 timestamps. The start must be in the future and
within seven days. The window must contain 1 to 48 whole hours. Optional `days`
sets observation history from 1 to 31 days and defaults to 3. Eligibility checks use
the window length, capped at 24 hours, and require forecast extent through `end`.
Temperatures use Fahrenheit.

The response has `stations` and `forecasts` arrays. Stations that do not meet
eligibility or forecast extent are excluded. Empty eligibility returns empty arrays
without scanning forecasts. Missing forecast fields remain null. An eligibility or
forecast query failure returns an error instead of a partial discovery response.

The endpoint accepts at most 5,000 server-selected stations and shares the existing
weather request admission limits. It does not accept arbitrary station IDs. These
display forecasts do not authorize settlement or guarantee future observations.

## Eligibility

`GET /stations/eligible` and this endpoint judge the same lists. After each
collection run, and a minute past each UTC midnight, the oracle reads the last 7
days of reports once and judges the default list (3 days, 24-hour windows) and every
cached list of up to 7 days from them. Any other history of up to 7 days and any
window length is judged from those reports in milliseconds. Longer histories read
the files on first use. A cached list is judged again in the background after the
next read or after 10 minutes, and requests get the previous list meanwhile.

A judged day on which fewer than half of the stations were clean was a collection
outage. It is not counted against any station, and `days_checked` leaves it out.
Below 20 reporting stations every day counts. Event creation does not rely on these
lists: settlement checks its own coverage.

`oracle_eligible_stations` counts the default list and changes only when it is
judged, not with request parameters.

## Admission

At most 8 weather requests run at once. Up to 128 more wait up to 10 seconds for a
turn. A request beyond that, or one that waits too long, gets `503` with
`Retry-After: 5`, and `oracle_weather_requests_turned_away_total` counts it.

Available from Oracle v2.7.0. Deploy this release before Coordinator v2.17.1.
