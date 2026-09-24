# Event and attestation contract

This is the contract between the oracle and any coordinator that builds DLCs
on its events. It is independent of the data being attested: NOAA weather is
one source, and other sources (space weather, tides, ...) use the same event,
entry, and attestation shapes. The dlctix construction is described in
[Ticketed DLCs](https://conduition.io/scriptless/ticketed-dlc/).

## Sources

`GET /oracle/sources` lists each source the oracle runs:

```json
[{
  "id": "noaa_weather",
  "default": true,
  "default_metrics": ["temp_high", "temp_low", "wind_speed"],
  "metrics": [
    {"id": "temp_high", "par": {"rule": "rounded"}},
    {"id": "rain_amt",  "par": {"rule": "within", "tolerance": 0.1}},
    {"id": "wind_direction", "par": {"rule": "compass", "tolerance": 22.0}}
  ]
}]
```

A source defines which targets exist (NOAA station ids), which metrics can
be predicted, and each metric's par rule. For every target and metric the
oracle reads a **baseline** (for NOAA, the forecast) and an **observed**
value over the event's observation window. A missing value is missing, never
zero, and earns no points.

| Par rule   | `Par` when                                   |
|------------|----------------------------------------------|
| `exact`    | observed = baseline                          |
| `rounded`  | both rounded to whole units are equal        |
| `within`   | \|observed − baseline\| ≤ tolerance          |
| `compass`  | angular distance on 360° ≤ tolerance         |

`Over` and `Under` compare the raw values (rounded ones for `rounded`).

### NOAA weather baseline

NOAA scoring uses the latest forecast issued strictly before the observation
window starts. The archive search covers the preceding seven days. Forecasts
issued during the event cannot replace this baseline.

Observation windows include the start instant and exclude the end instant.
Queries compare timestamps as instants and group daily values by UTC date.
File discovery also checks publications up to 24 hours after the requested
period, capped at the current time. Report timestamps must still fall inside
the observation window.

Repeated snapshots of the same station report count once. The latest
publication wins when a report changes. Missing precipitation remains missing;
a measured zero remains zero.

Multi-day baselines combine the matching UTC forecast days. Temperature highs
and wind speeds use maxima; temperature lows use minima; precipitation uses
sums. A missing day or metric leaves that baseline unavailable.
The current weather API requires temperature extrema for each returned row.
Rows without those extrema are unavailable even when other measurements exist.

Wind direction is the bearing reported with the maximum wind speed. Equal
speeds select the latest interval, then the latest UTC day for multi-day
forecasts. Missing direction at that maximum remains unavailable.

Forecast values retain their native intervals. An interval that overlaps an
event boundary can include weather outside the event. The oracle does not
prorate precipitation or infer hourly extrema from daily forecasts. UTC daily
windows provide the closest comparison with the daily baseline.

Observed humidity is derived from period temperature and dewpoint averages.
Its baseline remains the maximum forecast humidity. Observed snowfall uses
the existing 10:1 liquid-to-snow estimate rather than a direct snowfall measurement.

## Events

`POST /oracle/events`, signed by an allowlisted coordinator (NIP-98):

```json
{
  "id": "<uuidv7>",
  "source": "noaa_weather",
  "locations": ["KORD", "KSAW"],
  "scoring_fields": ["temp_high", "wind_speed"],
  "start_observation_date": "2030-01-01T00:00:00Z",
  "end_observation_date": "2030-01-02T00:00:00Z",
  "signing_date": "2030-01-02T03:00:00Z",
  "total_allowed_entries": 10,
  "number_of_places_win": 3,
  "number_of_values_per_entry": 4,
  "unlisted": false
}
```

`source` defaults to the default source and `scoring_fields` (alias
`metrics`) to the source's defaults; `targets` is accepted for `locations`.
`unlisted` (default `false`) keeps the event off the oracle's events page
and dashboard counts unless the reader chooses "Show unlisted"; its page,
`/events/{id}`, and the API still serve it, and responses carry the flag.
Limits: 2–25 entries, 1–5 places and fewer places than entries, at most
20,000 outcomes, 1–50 distinct targets, start < end ≤ signing date.

## Entries

`POST /oracle/events/{id}/entries`, signed by the event's coordinator,
submits every entry at once, before the observation window ends:

```json
{"event_id": "<id>", "entries": [
  {"id": "<uuidv7>", "event_id": "<id>", "picks": [
    {"target": "KORD", "metric": "temp_high", "prediction": "Over"}
  ]}
]}
```

Each pick predicts `Over`, `Par`, or `Under` the baseline for one target and
enabled metric, at most once per pair and at most
`number_of_values_per_entry` picks. NOAA events also accept the older
`expected_observations` form (`{"stations": "KORD", "temp_high": "Over"}`);
an entry uses one form or the other.

## Scoring

Per pick: `Par` earns 20 points, a correct `Over`/`Under` 10, otherwise 0.
An entry's base score is the sum over its picks. Its total score is
`max(1, base) × 10,000`. Entries rank by descending base score, then ascending
full UUIDv7 entry id. Earlier UUIDv7 timestamps win ties, including across
ten-second boundaries. Equal timestamps rank by the remaining UUID bits.

The total score retains its integer type and scale. Clients must use the entry
id to break equal scores. Previously signed events retain their stored scores
and attestations.

## Outcomes and announcement

Number the entries `0..n` in ascending id order. The outcomes, in order, are
every ordered choice of `places` winners from `0..n` (itertools
`(0..n).permutations(places)`), followed by the refund-all outcome
`[0, 1, …, n−1]`. The message for an outcome is each winner index as an
8-byte big-endian integer, concatenated.

The event response carries `nonce_point` `R` and
`event_announcement.locking_points`: locking point `i` is the dlctix
`attestation_locking_point(P, R, message_i)` for the oracle key `P`
(`GET /oracle/pubkey`). Clients can recompute every locking point from `P`,
`R`, and the outcome list. `event_announcement.expiry` is one day after the
signing date.

## Attestation

After the signing date the oracle ranks entries by the scoring rules, takes the
top `places` indices (or the refund-all outcome when every base score is 0),
and publishes `attestation` `s` with `s·G` equal to that outcome's locking
point. The oracle attests each event at most once and only an announced
outcome; the nonce behind `R` is derived from the oracle key and never
published. A client verifies an attestation by finding the `i` with
`locking_points[i] = s·G`.
