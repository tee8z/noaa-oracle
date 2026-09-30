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
    {"id": "temp_high", "par": {"rule": "rounded"}, "calibrated": true},
    {"id": "rain_amt",  "par": {"rule": "within", "tolerance": 0.1}, "calibrated": false},
    {"id": "wind_direction", "par": {"rule": "compass", "tolerance": 22.0}, "calibrated": false}
  ]
}]
```

`calibrated` metrics can also be scored against lines (see [Scoring](#scoring)).

A source defines which targets exist (NOAA station ids), which metrics can
be predicted, and each metric's par rule. For every target and metric the
oracle reads a **baseline** (for NOAA, the forecast) and an **observed**
value over the event's observation window. A missing value is missing, never
zero. During provisional scoring, a missing value earns no points. Settlement
requires a finite baseline and observation for every enabled target and metric.

| Par rule   | `Par` when                                   |
|------------|----------------------------------------------|
| `exact`    | observed = baseline                          |
| `rounded`  | both rounded to whole units are equal        |
| `within`   | \|observed − baseline\| ≤ tolerance          |
| `compass`  | angular distance on 360° ≤ tolerance         |

`Over` and `Under` compare the raw values (rounded ones for `rounded`).

### NOAA weather baseline

Strict NOAA scoring selects one latest eligible forecast issue and publication per station.
The issue must precede the window, and the recorded source receipt must be no later than its start.
The archive search covers the preceding seven days. A later forecast cannot replace this baseline.
A missing metric cannot fall back to a different, older issue.

Observation windows include the start instant and exclude the end instant.
Queries compare timestamps as instants and group daily values by UTC date.
File discovery also checks publications up to 24 hours after the requested
period, capped at the current time. Point measurements must still fall inside
the observation window. Precipitation also reads the report ending an accumulation at the window boundary.

Repeated snapshots of the same station report count once. The latest
publication wins when a report changes. Missing precipitation remains missing;
a measured zero remains zero.

Provisional display baselines combine the matching UTC forecast days.
Strict settlement baselines use validated native intervals over the full event window, separately for each metric.
Temperature highs and wind speeds use maxima; temperature lows use minima; precipitation uses sums.
Temperature extrema keep their native day or night periods.
Permitted edge gaps cover only the opposite daily period: 24 hours minus the native period's duration.
A cut native period or missing forecast day leaves that temperature metric unavailable.
Wind values are forecast samples inside the requested interval; direction belongs to the selected peak wind.
Precipitation requires a complete chain of native accumulation intervals with no gaps or boundary cuts.
Forecast rainfall also needs complete snow and ice components to separate liquid rain from total water equivalent.
Nonzero ice remains unavailable because accretion thickness is not a liquid-equivalent amount.
Nonzero snow needs its forecast ratio for the same native interval.
The public weather API still requires temperature extrema in its display rows.
Strict forecast values do not depend on that display constraint.

Wind direction is the bearing reported with the maximum wind speed. Equal
speeds select the latest interval, then the latest UTC day for multi-day
forecasts. Missing direction at that maximum remains unavailable.

Forecast values retain their native intervals. A boundary overlap can include weather outside the event.
The strict reader must establish a compatible interval before using a value for settlement.
The oracle does not prorate precipitation or infer hourly extrema from daily forecasts.
Choose event windows from compatible native ranges; a universal UTC-hour rule does not establish compatibility.

Strict observed humidity is the maximum relative humidity calculated from each report's temperature and dewpoint using the Magnus formula.
Its baseline is the maximum forecast humidity. Provisional display humidity still uses period temperature and dewpoint averages.
Strict observations require usable reports without gaps exceeding 90 minutes, including the event boundaries.
Optional wind, direction, and humidity values also need adequate reporting coverage.

Rain accumulations use the previous routine report as their reset anchor, as described in the
[AWC precipitation definitions](https://aviationweather.gov/help/data/).
Routine anchors must establish an hourly cadence between 45 and 75 minutes.
Overlapping special reports cannot be added repeatedly. Accepted intervals must cover the exact event window without gaps.
Trace amounts require review. Positive accumulations with unknown or mixed precipitation phase cannot become exact rainfall values.
A rainfall or snowfall value needs evidence for that metric; a missing value is not zero.
The provisional display's 10:1 snowfall estimate cannot authorize settlement.

Verified NWS RR7/SHEF reports provide an additional fixed UTC-hour precipitation source at supported stations.
The reader replays retained product text and station mappings, then requires an exact accumulation chain across the event window.
Fixed-hour totals still need adequate evidence for precipitation phase. Recent source availability does not guarantee future reports.

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
  "unlisted": false,
  "scoring_rules": "lines"
}
```

`source` defaults to the default source and `scoring_fields` (alias
`metrics`) to the source's defaults; `targets` is accepted for `locations`.
`unlisted` (default `false`) keeps the event off the oracle's events page
and dashboard counts unless the reader chooses "Show unlisted"; its page,
`/events/{id}`, and the API still serve it, and responses carry the flag.
Limits: 2–25 entries, 1–5 places and fewer places than entries, at most
20,000 outcomes, 1–50 distinct targets, start < end ≤ signing date.
Windows: the source must be able to attest the window from whole source
periods. For NOAA that is a window of at least 24 hours (any start; one
daytime high and one overnight low per day), or a 12-hour half: day,
12:00–24:00 UTC, which cannot score `temp_low`, or night, 00:00–12:00 UTC,
which cannot score `temp_high`. Neither half can score `humidity`. A
forecast period counts in the window that holds its midpoint: NOAA's
daytime highs are centred 17:00–23:00 UTC and overnight lows 05:30–11:30 UTC
for every US state.
`scoring_rules` is `fixed` (the default) or `lines`; see [Scoring](#scoring).
A `lines` event is refused when a metric is not `calibrated`, or when a
target and metric has no line yet. `lines_from_event` copies an earlier
event's lines instead of the current ones (see [Scoring](#scoring)). The
event response carries `scoring_rules`, and a `lines` event its `lines`.

Before funding, coordinators can check proposed station, metric, and window combinations through
[`GET /stations/window-compatibility`](settlement-operations.md#check-a-window-before-funding).
It returns forecast availability, reasons, and native boundaries. It does not change an event or promise future observations.

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

An event is scored with the rules it was created with.

**Fixed** (`"scoring_rules": "fixed"`, and every event created before lines
existed): per pick, `Par` earns 20 points under the metric's Par rule, a
correct `Over`/`Under` 10, otherwise 0.

**Lines** (`"scoring_rules": "lines"`): each target and metric has a line, a
band on the miss `observed − baseline`. `Under` when the miss is below
`lower`, `Over` when it is above `upper`, `Par` from `lower` to `upper`,
both included. Exactly one outcome happens, and a right pick earns 10
points, otherwise 0. Values are compared unrounded.

The oracle fits lines itself and refits them as windows end:

- History: for every station the source tracks, the baseline and observation
  of each past 24-hour window starting at 00:00 UTC, read with the
  provisional reader three hours after the window ends. The last 60 days
  are used.
- Fit: cuts sit midway between neighbouring past misses. Of the three cuts
  either side of the 1/3 and 2/3 marks, the pair whose largest outcome share
  is nearest a third is kept, lower cuts first on a tie. The airport audit's
  `walk_forward.py` (`fit_cuts`) does the same, and the oracle's tests check
  both give the same lines.
- A target with fewer than 20 past windows uses the pooled line of every
  station for that metric (`"level": "pooled"`).
- An event uses lines fitted on the window length nearest its own.

When an event is created, the oracle copies the current line for each of
its targets and metrics into the event, and scores it against those copies
only. Each line carries its history: how many windows, how many fell Over,
Par, and Under it, the first and last window, and when it was fitted.
`GET /oracle/lines?targets=KORD,KSAW&metrics=temp_high` shows the lines an
event created now would copy, and any target and metric without one.

Lines refit about twice a day, so events created hours apart would copy
different lines. A coordinator that splits one competition into several
events can create each with `"lines_from_event": "<earlier event id>"` so
they all score against the lines the first one froze. The oracle then copies
that event's lines, every field unchanged, instead of the current fit. The
new event must use `lines` rules, and the earlier event must be the same
coordinator's, use `lines` rules and the same source, and hold a line fitted
on the new event's window length for each of its targets and metrics; only
those lines are copied. Otherwise the event is refused with `400`.

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

## Signed event statement

A locking point depends only on `P`, `R`, and the outcome's message; a message
names no event. So a party that checks a contract without seeing its event,
such as a verifier inside an enclave, needs proof that `R` belongs to an event
with the outcomes and terms it expects. Otherwise it could be handed the nonce
point of another event, for example one already attested.

Once every entry is in, `GET /oracle/events/{id}` carries `statement`:

```json
{"statement": {
  "event_id": "<id>",
  "signing_date": 1790172800, "expiry": 1790259200,
  "nonce_point": "<R, compressed hex>",
  "outcomes": {
    "kind": "ranking", "number_of_places_win": 1,
    "entry_ids": ["<entry 0>", "<entry 1>", "<entry 2>"]
  },
  "terms": {
    "kind": "observation", "source": "noaa_weather",
    "start_observation_date": 1790000000, "end_observation_date": 1790086400,
    "targets": ["KORD", "KSAW"], "scoring_fields": ["temp_high", "temp_low", "wind_speed"],
    "number_of_values_per_entry": 3,
    "scoring_rules": "lines",
    "lines": [
      {"target": "KORD", "metric": "temp_high", "lower": -2.5, "upper": -0.5, "window_hours": 24}
    ]
  }
 },
 "signature": "<BIP340 signature, hex>"}
```

A statement has three layers:

- **Core**, which every DLC on the event needs: the event, when it is attested,
  its expiry, and its nonce point.
- **`outcomes`**: how the outcomes are listed and which message each attests.
  Every event ranks its entries today (`ranking`): `entry_ids` are in ascending
  id order, so entry `i` is outcome index `i`, and the outcomes are those of
  [Outcomes and announcement](#outcomes-and-announcement).
- **`terms`**: what the event measures and how it is judged. Every event is an
  `observation` event today: predictions scored against a source's observed
  values over a window. Its `scoring_rules` and, for `lines` events, the lines
  frozen at creation (sorted by target, then metric) decide every pick; see
  [Scoring](#scoring). A `fixed` event has no lines and scores by the source's
  rules.

Each of `outcomes` and `terms` is tagged by `kind`. Later kinds extend a
statement, for example tiers of a measured value for parametric cover, or
other sources such as tides or space weather. A client must refuse a kind or
field it does not know. Dates are UNIX seconds, rounded down.

The signature is BIP340 by the oracle key `P` over the tagged hash
`SHA256(SHA256(tag) || SHA256(tag) || message)` with tag
`noaa-oracle/statement/v1`. `message` is the core, then each part's kind and
fields:

| Field | Encoding |
| --- | --- |
| `event_id` | 16 bytes |
| `signing_date` | 8-byte big-endian signed integer |
| `expiry` | 4-byte big-endian |
| `nonce_point` | 33 bytes, compressed |
| `outcomes.kind` | string |
| `ranking`: `number_of_places_win` | 4-byte big-endian |
| `ranking`: `entry_ids` | 2-byte big-endian count, then 16 bytes each |
| `terms.kind` | string |
| `observation`: `source` | string |
| `observation`: `start_observation_date`, `end_observation_date` | 8-byte big-endian signed integers each |
| `observation`: `targets`, `scoring_fields` | 2-byte big-endian count, then each string |
| `observation`: `number_of_values_per_entry` | 4-byte big-endian |
| `observation`: `scoring_rules` | string: `fixed` or `lines` |
| `observation`: `lines` | 2-byte big-endian count, then each line |
| each line: `target`, `metric` | strings |
| each line: `lower`, `upper` | 8-byte big-endian IEEE 754 doubles, the exact values the event scores against |
| each line: `window_hours` | 4-byte big-endian |

A string is its UTF-8 length as a 2-byte big-endian integer, then its bytes.
A new kind adds its own fields after its name; an existing kind's encoding
never changes, and a changed one gets a new kind name. The oracle signs
without auxiliary randomness, so a statement always carries the same
signature. The tag keeps these signatures apart from attestations and nostr
events made with the same key.

A verifier checks the signature against `P`, checks every field against what
it expects, and derives the locking points from `P`, `R`, and the outcome
messages. The Rust type `oracle::statement::Statement` does this with
`locking_points`.

## Attestation

After the signing date, the oracle rereads validated forecasts and observations.
The source must prove collection coverage for every station across the event window.
Missing, rejected, unverified, or incomplete readings block signing before saved readings or scores change.
Every enabled target and metric must have exactly one finite baseline and observation.

With complete data, the oracle ranks entries by the scoring rules and takes the
top `places` indices (or the refund-all outcome when every base score is 0),
and publishes `attestation` `s` with `s·G` equal to that outcome's locking
point. The oracle attests each event at most once and only an announced
outcome; the nonce behind `R` is derived from the oracle key and never
published. A client verifies an attestation by finding the `i` with
`locking_points[i] = s·G`.

Final readings, entry scores, the attestation, and clearing the blocked reason commit in one SQLite transaction.
Competing finalizations cannot mix their evidence. Delayed provisional updates cannot change a signed event's readings or scores.

### Blocked settlement

Unsigned event details and summaries can include `settlement_block`:

```json
{
  "code": "incomplete_readings",
  "message": "settlement blocked: verified baseline and observation required for KORD/rain_amt",
  "checked_at": "2030-01-02T03:05:00Z"
}
```

The reason persists in SQLite and appears on the event page and events list.
The processing job retries automatically, waiting longer after each failure in a row. Stored readings and scores remain provisional while settlement is blocked.
A successful settlement check rereads the data, recomputes scores, and clears the reason.
Signed events retain their stored readings, scores, and attestation.

An event that reaches its signing date without entries has no outcome, and is never attested.
Its details and summaries then carry `settled_without_entries_at`, the time the oracle closed it; `status` stays `Completed` and `attestation` stays null.

An empty result is not proof that all predictions were wrong.
A source outage or missing station therefore cannot produce a signed refund-all outcome.
The announced expiry remains available to the coordinator and participants under their DLC contract.

Precipitation and other configured metrics remain eligible when their requested intervals have verifiable values.
The oracle does not change an announced observation window to make missing data fit.
See [settlement operations](settlement-operations.md) for rollout and blocked-event checks.
