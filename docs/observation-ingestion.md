# Observation history and evidence

The daemon collects METAR history from the [official AWC data API](https://aviationweather.gov/data/api/). Each run requests the configured station catalog over a bounded lookback. The default is four hours, with 25 stations per request. This replaces the latest-report cache, which could lose routine reports and special reports between hourly collection runs.

Observation collection and publication run before the independent forecast task. The METAR history step can take up to 20 minutes. The separate RR7 precipitation step can add up to 10 minutes before the combined observation artifact publishes. A forecast failure does not remove an observation artifact that has already been published. A station catalog failure creates a zero-row observation artifact with a failed receipt. It does not certify an empty source.

## Collection limits

`observation_history_hours` accepts 1 through 24 hours. It must exceed `sleep_interval`. `observation_batch_size` accepts 1 through 25 stations. The daemon also enforces these limits:

| Limit | Behavior |
| --- | --- |
| 400 reports in a response | Split the station batch, then split the time window for a single station. The capped parent receipt certifies no coverage. |
| 512 requests per run | Record the remaining query intervals as failed. |
| 20 minutes per history run | Stop requests and record remaining intervals as failed. |
| 64 MiB of received source bodies per run | Stop collection and record a size-budget failure. |
| 750 ms minimum between request starts | Apply this spacing in addition to the configured shared rate limiter. |
| 60 seconds per physical request | Record a transport failure. Retry the interval in the next collection run. |

History requests do not use automatic retry middleware. Each physical request passes through the rate limiter. The four-hour default accommodates a two-hour signing grace, hourly collection, and normal collection latency. Longer signing grace requires a larger lookback.

The next run's lookback no longer reaches the oldest hour of a failed batch. The daemon therefore keeps every failed query interval, except a capped parent, and requests it again after the next run's own window. It requests only the part before that window. A station catalog failure is retried for the next run's whole catalog. An interval leaves the list when it succeeds or when it ends more than 26 hours before the run. At most 64 retries run per collection, newest first, within the same request and time budget. The list is held in memory, so a daemon restart forgets it. An outage longer than the lookback, or a failure the retries never repair, can leave a permanent coverage gap in the local retained history. The oracle must hold settlement for that gap. It must not infer zero weather activity from missing collection.

The requested inner interval is closed at both ends. It ends one second before the collection run starts, rounded to a whole second. The HTTP query includes one extra second at each end because the API does not document boundary inclusivity. Rows outside the inner interval do not expand the certified coverage. Overlapping runs can contain the same report; consumers must deduplicate before aggregation.

## Parquet evidence

Each observation file has an `observation_coverage` JSON footer, including files with zero rows. The footer uses version `awc-history-v1` and interval `closed`. It records the run window and completion time. Each batch records its station list, requested window, request start and completion times, source URL, source body SHA-256, HTTP status, source report count, retained report count, status, and any error.

A batch status has these meanings:

| Status | Meaning |
| --- | --- |
| `complete` | HTTP 200 returned a valid, uncapped response. Its report count, station identities, timestamps, and required representable fields passed collection checks. Individual weather values can still be rejected by quality checks. |
| `empty` | HTTP 204 had an empty body, or valid HTTP 200 XML declared and contained no reports. This certifies the query, not an observed zero value. |
| `failed` | The batch cannot certify coverage. The error and `failure_kind` explain why. |

`failure_kind` is null for successful batches. Failed batches use `transport`, `response`, `representation`, `budget`, or `cap`. A transport failure leaves a coverage gap. A received malformed response or unrepresentable report is additional data-quality evidence. Consumers must distinguish those cases when evaluating previous successful coverage.

The `observation_audit` footer contains the original source documents in `source_documents`, keyed by the SHA-256 of the received UTF-8 body. Each document is gzip compressed and base64 encoded. Batch receipts retain the URL for each request, including requests whose response bodies have identical hashes. HTTP error bodies are also retained when available and within the evidence budget. Transport failures with no body and responses that exceed a size limit have an error receipt but cannot provide a complete body archive.

The audit's top-level `source_sha256` hashes the serialized source-document map. `source_hash_kind` is `source_documents-json-sha256`; it is not the hash of one XML response. The same audit is written and synced to a local `.parquet.quality.json` sidecar before the parquet file is created. The parquet footer carries the evidence through normal upload and archive operations. Published sidecars expire with their parquet files. Orphan sidecars follow the configured retention period.

## Report quality and precipitation

The append-only observation schema retains `raw_text`, `quality_status`, `quality_reason`, `validation_version`, `metar_type`, and `quality_metrics`. Convertible rejected reports keep their original decoded values. The daemon does not substitute a plausible reading for a contradictory source value. A report with unrepresentable required coordinates or time cannot become a parquet row; the archived response and failed batch receipt preserve the evidence.

Any problem makes a report `rejected` (or `unverified` when no unambiguous raw temperature group exists). `quality_metrics` lists, comma-separated, the metric groups the report's problems affect:

| Group | Problems |
|---|---|
| `temperature`, `dewpoint` | Invalid or missing decoded values, a conflict with the raw body or T group, or no unambiguous raw temperature evidence |
| `wind` | Invalid, out-of-range, or raw-inconsistent sustained wind speed or direction |
| `precipitation` | Rain gauge outage (`PNO`), unavailable or conflicting `P` groups, or SPECI amounts without one |
| `present_weather` | Present-weather sensor outage (`PWINO`) |
| all groups | A station, time, or report type that disagrees with the raw text |

The column is null for validated reports and in files written before it existed. The oracle counts a flagged report against an event only when these groups overlap the event's metrics, and it drops only the affected values; see [settlement operations](settlement-operations.md#observation-quality-by-metric).

`validated` means the configured source consistency checks passed. It does not certify a complete event window or rule out every physical or regional anomaly. The oracle must also apply its quality and coverage checks before signing.

The original METAR/SPECI type is retained for precipitation interval construction. The ASOS `P####` remark reports precipitation since the previous scheduled hourly computation. A special report can repeat a prefix of the same accumulation interval. Consumers must not sum overlapping prefixes or treat each report as an independent rolling hour. See the [ASOS User's Guide](https://www.weather.gov/media/asos/aum-toc.pdf), printed pages 20–21. The oracle requires defensible interval anchors and complete interval coverage before scoring precipitation.

The daemon and oracle must deploy together. Older oracle readers can ignore appended columns and footer metadata, so they cannot enforce the new quality and coverage policy.


## Aligned hourly precipitation

The daemon also collects official NWS ASOS RR7 SHEF reports. [NWS defines their PPH field](https://www.weather.gov/asos/InformationReporting.html) as a discrete 60-minute precipitation accumulation ending at the top of the UTC hour. This gives the oracle measured intervals that can match native NDFD precipitation periods without shifting a METAR timestamp or prorating an amount.

The collector discovers current RR7 product locations at `https://api.weather.gov/products/types/RR7/locations`. It maps each location through the exact station ID or IATA alias in the retained official AWC station catalog. Ambiguous aliases remain unavailable. It does not add a `K` prefix to guess an ICAO identifier. A bounded product index request selects only mapped locations and the requested history. A 500-result index is split by time before its product IDs can be trusted as complete.

The NWS collector uses a separate one-request-per-second limit, with no automatic retries, at most 512 requests, a ten-minute deadline, and an 8 MiB total evidence budget. Newer products are fetched first. Failures do not prevent METAR publication. The four-hour default normally needs several hundred small product fetches across the currently supported stations. A larger lookback or outage can exceed the budget; missing hours remain unavailable and cannot become zero.

The `precipitation_observations` footer uses version `asos-shef-v1`. It contains `rows`, `sources`, `supported_stations`, request/completion timestamps, and `issues`. Each row identifies its station, source station alias, UTC start and end, amount, status and reason, source URL and SHA-256, issue and receipt times, and the station catalog SHA-256 used for mapping. The source map retains gzip/base64 encoded discovery JSON, product indexes, product JSON with original `productText`, and station catalog XML. The top-level keys are SHA-256 hashes of those original received UTF-8 bodies.

The same pure parser in `noaa_oracle_core::shef` is used for ingestion and independent settlement replay. It accepts the documented ASOS hourly `.A`/`.AR` PPH subset with default or explicit UTC, and rejects unsupported timezones or period modifiers. Missing values and trace amounts are not zero. Malformed, negative, nonfinite, and unusually large values remain rejected evidence. A missing P group in SPECI also remains unknown, including when the upstream decoder supplies zero.

RR7 coverage is station-specific. The current discovery list and catalog permit KDSM, KMCI, and KMSP. KNYC currently lacks an explicit NYC catalog alias and remains unavailable. These sources do not make every ASOS station available. The oracle must require an exact, verified interval chain and usable phase evidence; a positive liquid-equivalent total does not itself prove liquid-only rainfall. Future planning must expose station capability and unresolved interval requirements before funding.

For a bounded read-only probe, run `cargo run -p daemon --example audit_shef -- KMCI`. It collects only that station's RR7 history and prints the evidence manifest. It does not publish data or attest an outcome.
