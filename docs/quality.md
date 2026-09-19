# NOAA Oracle engineering quality guide

Apply these conventions to new and changed code. Explain exceptions in review.
See the [README](../README.md) for architecture and the
[Helm chart](../deploy/helm/noaa-oracle/values.yaml) for deployment settings.

## Design and types

- Start with concrete types and inherent `impl` blocks. Add a trait only for a
  production substitution boundary (`FileData` for local disk or S3,
  `WeatherData` for parquet queries) or behaviour shared by an algorithm
  (`OutcomeSource`: scoring and attestation are shared by every data source).
- Pass dependencies through constructors (`AppState::new`, `Oracle::new`).
  Keep operation order, ownership, and failure behaviour explicit.
- Expose fields on plain records. Keep fields that protect invariants private
  and avoid trivial getters.
- Use enums with typed payloads for alternatives and match them exhaustively.
- Wire types in [events.rs](../crates/oracle/src/events.rs) are part of the
  coordinator contract. Keep field names, optionality, and RFC 3339 dates
  stable; the [contract test](../crates/oracle/tests/api/coordinator_contract.rs)
  posts the coordinator's exact JSON. The outcome order (permutations of
  `places` winners, then refund-all) and the outcome message encoding in
  [scoring.rs](../crates/oracle/src/scoring.rs) are part of it too.

## Module boundaries

- [database.rs](../crates/oracle/src/database.rs) owns connections, SQL, and
  write commands. Handlers and the ETL use `Database` operations and never
  see a connection.
- [startup.rs](../crates/oracle/src/startup.rs) composes the process: state,
  router, task supervision, and shutdown ordering. `main.rs` stays small.
- [routes/](../crates/oracle/src/routes/) maps errors to status codes;
  [auth.rs](../crates/oracle/src/auth.rs) verifies NIP-98 writers;
  [events.rs](../crates/oracle/src/events.rs) holds every event and entry
  rule; [oracle.rs](../crates/oracle/src/oracle.rs) orchestrates creation,
  scoring, and attestation; [signing.rs](../crates/oracle/src/signing.rs)
  owns the key and nonces; [sources/](../crates/oracle/src/sources/) adapts
  data sources; [weather_data.rs](../crates/oracle/src/weather_data.rs) holds
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

## Security

- The attestation nonce never leaves `signing.rs`: derive it, never store or
  serve it. Attest at most once per event and only announced outcomes; the
  database refuses to replace an attestation.
- Secrets get dedicated types without `Clone`, `Debug`, or `Serialize`;
  zeroize owned buffers. Key files are created 0600 and refused when
  readable by group or others.
- Every write is authenticated (`auth::Signed`) and authorized explicitly in
  the handler (`AuthPolicy::require`). Check signatures against the
  configured `remote_url`, never the `Host` header.
- Bound untrusted work: request bodies (256 KiB, 64 MiB for uploads), event
  size and outcome count, list limits, and anything CPU heavy runs off the
  async runtime.
- Published data files are immutable once uploaded.
- Missing source values stay missing. Verify semantics against the source's
  documentation before defaulting anything (NOAA nils are not zero).

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
  flight, the coordinator wire contract, single attestation, and every auth
  rejection (missing, stale, replayed, wrong URL, payload mismatch, not
  allowed).
- Inject time (`oracle::Clock`) instead of sleeping or backdating events.
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
