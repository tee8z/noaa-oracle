# oracle

REST API, HTMX dashboard, and DLC attestation service. See the
[repository README](../../README.md) for setup and the
[quality guide](../../docs/quality.md) for conventions.

## Endpoints

| Route | Purpose |
| --- | --- |
| `GET /health`, `GET /ready` | Readiness: writer accepts commands and SQLite answers a read; 503 while shutting down |
| `GET /healthy` | Liveness: HTTP only |
| `GET /docs` | OpenAPI UI |
| `GET /files`, `GET /file/{name}`, `POST /file/{name}` | List, download, and upload parquet files (`<observations|forecasts>_<rfc3339>.parquet`) |
| `GET /stations`, `/stations/forecasts`, `/stations/observations`, `/stations/daily-observations` | Weather queries over parquet files |
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
```

## Storage

- `event_db/events.sqlite`: one writable connection behind a bounded command
  queue, plus a read-only pool. See [database.rs](src/database.rs).
- `weather_dir/YYYY-MM-DD/*.parquet`: queried in-process with DuckDB. The
  crate links against `libduckdb`; `nix develop` provides the matching
  version, or set `DUCKDB_LIB_DIR` / build with `--features bundled`.
