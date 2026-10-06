# oracle

REST API, HTMX dashboard, and DLC attestation service. See the
[repository README](../../README.md) for setup and the
[quality guide](../../docs/quality.md) for conventions.

## Endpoints

| Route | Purpose |
| --- | --- |
| `GET /health` | Health: writer accepts commands and SQLite answers a read; 503 while shutting down |
| `GET /ready` | Readiness to take traffic: `/health`, and the first preparation of recent forecast files (copies and folds) has finished; 503 until then |
| `GET /healthy` | Liveness: HTTP only |
| `GET /docs` | OpenAPI UI |
| `GET /files`, `GET /file/{name}`, `POST /file/{name}` | List, download, and upload parquet files (`<observations|forecasts>_<rfc3339>.parquet`) |
| `GET /stations`, `/stations/forecasts`, `/stations/observations`, `/stations/daily-observations` | Weather queries over parquet files: 1 to 100 stations and at most 31 days a query. `/stations/forecasts` reads forecasts issued in the last 10 days; older issues are in the files `/files` lists (the private listener still reads them) |
| `GET /stations/eligible?days=3&window_hours=24` | Stations a competition of `window_hours` (1 to 24) starting now can be drawn from: on all but one in ten (and at least all but one) of the last `days` (1 to 3; 1 to 31 on the private listener) full UTC days, the reports settlement reads covered every window of that length starting on the hour, the newest report is under 3 hours old, and the newest forecast runs at least `window_hours` past now. Ordered by station id; lists are rebuilt at most every 10 minutes; unknown parameters or values out of range get 400 |
| `GET /stations/eligible/forecasts`, `GET /stations/window-compatibility` | Discovery and window planning (see [discovery](../../docs/discovery.md) and [settlement operations](../../docs/settlement-operations.md#check-a-window-before-funding)); answers are kept until new data arrives. Building one is heavy work: at most 2 run at once, and requests that find no turn get 503 with `Retry-After` |
| `GET /oracle/pubkey`, `GET /oracle/npub` | Oracle identity |
| `GET /oracle/events`, `POST /oracle/events`, `GET /oracle/events/{id}` | DLC events (writes require NIP-98 auth) |
| `POST /oracle/events/{id}/entries`, `GET /oracle/events/{id}/entries/{entry}` | Entries |
| `POST /oracle/update` | Start one ETL pass (202); 409 if one is running, 503 during shutdown |

## Examples

```sh
curl "http://localhost:9800/files?start=2024-01-15T00:00:00Z&end=2024-01-16T00:00:00Z&forecasts=true"
curl -L -O http://localhost:9800/file/observations_2024-01-14T04:44:22.246930703Z.parquet
curl -F "file=@forecasts_2024-01-14T04:44:22.246930703Z.parquet" \
  http://localhost:9800/file/forecasts_2024-01-14T04:44:22.246930703Z.parquet
curl "http://localhost:9800/stations/observations?start=2024-02-15T00:00:00Z&end=2024-02-25T00:00:00Z&station_ids=KLWV,KLBB,KTOA"
curl "http://localhost:9800/stations/eligible?days=3&window_hours=12"
# Longer histories, on the private (operator) listener only:
curl "http://127.0.0.1:9801/stations/eligible?days=30&window_hours=12"
```

## Storage

- `event_db/events.sqlite`: one writable connection behind a bounded command
  queue, plus a read-only pool. See [database.rs](src/database.rs).
- `weather_dir/YYYY-MM-DD/*.parquet`: queried in-process with DuckDB. The
  crate links against `libduckdb`; `nix develop` provides the matching
  version, or set `DUCKDB_LIB_DIR` / build with `--features bundled`.
