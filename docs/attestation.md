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
  "number_of_values_per_entry": 4
}
```

`source` defaults to the default source and `scoring_fields` (alias
`metrics`) to the source's defaults; `targets` is accepted for `locations`.
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
`max(1, base) × 10,000 − (entry creation milliseconds mod 10,000)`, taken
from the UUIDv7, so earlier entries win ties; remaining ties rank by entry
id.

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

After the signing date the oracle ranks entries by total score, takes the
top `places` indices (or the refund-all outcome when every base score is 0),
and publishes `attestation` `s` with `s·G` equal to that outcome's locking
point. The oracle attests each event at most once and only an announced
outcome; the nonce behind `R` is derived from the oracle key and never
published. A client verifies an attestation by finding the `i` with
`locking_points[i] = s·G`.
