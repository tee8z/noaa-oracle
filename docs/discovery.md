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

Available from Oracle v2.7.0. Deploy this release before Coordinator v2.17.1.
