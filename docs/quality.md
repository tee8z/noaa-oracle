# NOAA Oracle engineering quality guide

Apply these conventions to new and changed code. Explain exceptions in review.
See the [README](../README.md) for architecture and the
[Helm chart](../deploy/helm/noaa-oracle/values.yaml) for deployment settings.

## Design and types

- Start with concrete types and inherent `impl` blocks. Add a trait only for a
  production substitution boundary (`FileData` for local disk or S3,
  `WeatherData` for parquet queries) or behaviour shared by an algorithm.
- Pass dependencies through constructors (`AppState::new`, `Oracle::new`).
  Keep operation order, ownership, and failure behaviour explicit.
- Expose fields on plain records. Keep fields that protect invariants private
  and avoid trivial getters.
- Use enums with typed payloads for alternatives and match them exhaustively.
- Wire types in [events.rs](../crates/oracle/src/events.rs) are part of the
  coordinator contract. Keep field names, optionality, and RFC 3339 dates
  stable; the [contract test](../crates/oracle/tests/api/coordinator_contract.rs)
  posts the coordinator's exact JSON.

## Module boundaries

- [database.rs](../crates/oracle/src/database.rs) owns connections, SQL, and
  write commands. Handlers and the ETL use `Database` operations and never
  see a connection.
- [startup.rs](../crates/oracle/src/startup.rs) composes the process: state,
  router, task supervision, and shutdown ordering. `main.rs` stays small.
- [routes/](../crates/oracle/src/routes/) maps errors to status codes;
  [oracle.rs](../crates/oracle/src/oracle.rs) holds event rules, scoring, and
  attestation; [weather_data.rs](../crates/oracle/src/weather_data.rs) holds
  DuckDB queries over parquet files.
- Keep items private by default. Re-export from `lib.rs` only what the binary
  and integration tests use.

## Database and lifecycle

- One writable SQLite connection lives in `DatabaseWriter`. Handlers submit
  commands to a bounded queue (64) and read through a read-only, query-only
  pool. A reply arrives only after the transaction commits.
- Queue full or closed rejects a write before admission (HTTP 503). A lost
  reply after admission is an unknown outcome (HTTP 500); never retry it
  blindly. Accepted writes finish even if the caller disconnected.
- Request-path writes use `try_send`; the ETL waits for capacity because it
  has already done bounded work.
- Add numbered migrations under `crates/oracle/migrations/` without changing
  applied files. Existing databases keep their data on reopen.
- Shutdown order: disable readiness, drain HTTP, stop background producers
  (cache warming, ETL), cancel the writer and let it drain, then close SQLite.
  Every step is bounded by `shutdown_timeout`; the Helm termination grace
  period must exceed it.
- Litestream replicates asynchronously. A successful write confirms a local
  commit only.

## Validation and errors

- Validate external input at the boundary: station ids before SQL
  (`validate_station_id`), parquet file names before paths and SQL
  (`ParquetFileName::parse`), configuration before opening anything
  (`Cli::configuration`).
- Use typed errors when callers act differently (`WriteError`,
  `oracle::Error`). Log details with `log`; return safe messages to clients.
- Do not panic on external input or database rows; decode with `try_get` and
  map failures to `sqlx::Error::ColumnDecode`.

## Tests and documentation

- Name tests after observable behaviour and keep them beside the owning
  module. Use real SQLite databases in temporary directories.
- Prove guarantees: commit before reply, queue rejection, dropped replies,
  read-only readers, migration preservation, shutdown drain, ETL single
  flight, and the coordinator wire contract.
- Daemon XML parsing is tested against trimmed NOAA fixtures in `testdata/`
  directories next to the modules.
- Document invariants and surprising decisions once, where they live.

## Dependencies and checks

- Add dependencies only for current needs and enable only required features.
  `rust-toolchain.toml` pins the toolchain for rustup and Nix; `flake.nix`
  pins the DuckDB library to the version the `duckdb` crate binds.
- Run the local gate before submitting code or deployment changes:

```sh
just check   # fmt, clippy -D warnings, tests, cargo-machete
```

CI additionally runs `nix flake check` and the Playwright UI suite.
