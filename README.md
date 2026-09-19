# NOAA Oracle

A data pipeline system that fetches weather data from NOAA and serves it via a REST API with DLC (Discreet Log Contract) attestation support.

- Live site: [4casttruth.win](https://www.4casttruth.win/)
- Feel free to pull the parquet files and use in your own data analysis

## Architecture

```
[NOAA API] <- [daemon] -> parquet files -> [oracle] <- parquet files <- [browser DuckDB]
```

**Components:**
- **daemon** - Background process that pulls data from NOAA, transforms it into parquet files, and pushes to the oracle
- **oracle** - REST API that stores parquet files, serves them via browser UI, tracks DLC events in SQLite, and signs attestations
- **ui** - Browser interface using DuckDB-WASM for client-side querying of parquet files
- **core** - Shared library for configuration loading and utilities

**Event storage:** the oracle keeps DLC events in one SQLite database. One
writer task owns the only writable connection and runs commands from a
bounded queue. HTTP handlers read through a read-only pool and receive a
reply only after a commit. A full queue answers HTTP 503, so the coordinator
retries later. On shutdown the oracle stops readiness, drains HTTP and
background work, then drains and closes the writer. Litestream (optional)
replicates the file asynchronously, so a successful write confirms a local
commit only. See [docs/quality.md](docs/quality.md).

## Quick Start

### Using Nix (Recommended)

`rust-toolchain.toml` pins the Rust toolchain for both rustup and the Nix
shell, and the flake provides the DuckDB library version the `duckdb` crate
expects.

```bash
# Enter development shell
nix develop

# Build both binaries
cargo build --workspace --locked

# Or use just commands
just build
```

### Without Nix

The oracle crate links against the DuckDB C library. Find the required DuckDB version in the `duckdb` line of `crates/oracle/Cargo.toml`. Download that release (for example `libduckdb-linux-amd64.zip`) from [DuckDB releases](https://github.com/duckdb/duckdb/releases) and point to it:

```bash
# Extract and set environment variables
export DUCKDB_LIB_DIR=/path/to/libduckdb
export LD_LIBRARY_PATH=$DUCKDB_LIB_DIR:$LD_LIBRARY_PATH
cargo build --workspace
```

Alternatively, use `--features oracle/bundled` to compile DuckDB from source (much slower build).

### Running the Services

```bash
# Run daemon (fetches NOAA data)
just run-daemon

# Run oracle (serves data and API)
just run-oracle

# Run oracle with pre-existing weather data
just run-oracle-standalone /path/to/weather/data
```

## Configuration

Configuration follows XDG Base Directory Specification. Files are searched in order:

1. Environment variable (`ORACLE_CONFIG` / `DAEMON_CONFIG`)
2. Current directory (`./oracle.toml` / `./daemon.toml`)
3. XDG config (`~/.config/noaa-oracle/oracle.toml`)
4. System config (`/etc/noaa-oracle/oracle.toml`)

Example configurations are in the `config/` directory:
- `config/oracle.example.toml`
- `config/daemon.example.toml`

### Oracle Configuration

See [config/oracle.example.toml](config/oracle.example.toml) for every
setting. The essentials:

```toml
host = "0.0.0.0"
port = 9800
# Public origin that NIP-98 signatures are made over
remote_url = "https://oracle.example.com"
data_dir = "/var/lib/noaa-oracle/weather"
event_db = "/var/lib/noaa-oracle/events"
private_key_path = "/etc/noaa-oracle/keys/oracle.pem"
# Nostr keys (npub or hex) allowed to write
coordinator_pubkeys = ["npub1..."]
uploader_pubkeys = ["npub1..."]
```

`GET /health` reports readiness (writer available and a database read succeeds) and turns 503 during shutdown; `GET /healthy` reports HTTP liveness only.

### Security model

- **Attestations.** Each event commits to a nonce point `R`; its nonce is
  derived from the oracle key, the event id, and a random per-event salt at
  signing time and is never stored or served. The oracle attests at most
  once per event and only an outcome listed in the event's announcement.
- **Writes.** Event creation and entry submission need a NIP-98 signature
  from an allowlisted coordinator; data uploads and `POST /oracle/update`
  need an allowlisted uploader (the daemon). The signed URL must match
  `remote_url`, the `payload` tag must match the body, and each signed
  event is accepted once.
- **Keys.** The key file must be mode 0600 (or 0400); the oracle refuses
  wider permissions and erases the key from memory on exit.
- **Uploads.** Files are raw parquet bodies, written atomically, and never
  replaced once published.

### Adding a data source

The pipeline is source independent: a daemon publishes parquet files, the
oracle reads them, scores entries against a baseline, and attests the
ranking. To attest something other than NOAA weather, implement
`OutcomeSource` (`crates/oracle/src/sources/`) for the oracle side: valid
targets, metrics with their "par" rules, and baseline/observed readings for
an observation window. Scoring, ranking, announcements, and attestation are
shared. On the daemon side, implement its `Source` trait to fetch and write
the parquet datasets. The event, entry, and attestation contract is the same for
every source; see [docs/attestation.md](docs/attestation.md).

### Daemon Configuration

```toml
level = "info"

# Oracle to upload to. Must equal the oracle's `remote_url`: uploads are
# signed over the request URL.
base_url = "http://localhost:9800"

# Parquet files stay here until published and older than retention_days
data_dir = "/var/cache/noaa-oracle"

# Fetch interval in seconds (NOAA updates hourly)
sleep_interval = 3600

# Upload signing key, created with mode 0600 if missing. The daemon logs
# its npub at startup; list it in the oracle's `uploader_pubkeys`.
private_key = "/var/cache/noaa-oracle/keys/daemon.pem"

# Discard a run when fewer than this fraction of stations got a forecast
min_forecast_coverage = 0.8
retention_days = 7
```

Each run writes `forecasts_<rfc3339>.parquet` and
`observations_<rfc3339>.parquet`, uploads them as signed raw parquet
bodies, and keeps any file the oracle has not accepted for retry on the
next run. A run that is too incomplete, fails, or times out is discarded
rather than published. See `config/daemon.example.toml` for every setting.

### Upgrading from 1.x

Uploads are now signed, so a 1.x daemon cannot publish to a 2.x oracle and
a 2.x daemon cannot publish to a 1.x oracle. Upgrade both together:

1. Start the new daemon once. It creates its signing key at `private_key`
   (default `./daemon_private_key.pem`, mode 0600) and logs
   `add this npub to the oracle's uploader_pubkeys: npub1...`. Back the
   key up like the oracle's; a new key means a new npub.
2. Put that npub in the oracle's `uploader_pubkeys` (config file,
   `NOAA_ORACLE_UPLOADER_PUBKEYS`, the Helm chart's
   `config.uploaderPubkeys`, or the NixOS module's `uploaderPubkeys`) and
   restart the oracle.
3. Set the daemon's `base_url` to exactly the oracle's `remote_url`.
   Signatures cover the request URL, so a different scheme, host, or port
   is rejected with 401.
4. Drop any key from `daemon.toml` that `config/daemon.example.toml` does
   not list; unknown keys are now a startup error.

Kubernetes: give the daemon chart its key through
`secrets.privateKey.existingSecret` (a secret holding `daemon.pem`);
without it the key, and so the npub, changes on every pod restart. NixOS:
the module creates the key under the daemon's data directory; only the
npub step is manual.

## NixOS Deployment

Add to your NixOS configuration:

```nix
{
  inputs.noaa-oracle.url = "github:tee8z/noaa-oracle";

  outputs = { self, nixpkgs, noaa-oracle, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        noaa-oracle.nixosModules.default
        {
          services.noaa-oracle = {
            enable = true;
            oracle = {
              enable = true;
              host = "0.0.0.0";
              port = 9800;
              # The daemon's npub, logged when it starts
              uploaderPubkeys = [ "npub1..." ];
            };
            daemon = {
              enable = true;
              interval = 3600;
            };
          };
        }
      ];
    };
  };
}
```

## Development

```bash
# Enter dev shell
nix develop

# Format code
just fmt

# Local gate: fmt-check, clippy -D warnings, tests, cargo-machete
just check

# Only the oracle HTTP API tests
just test-api

# Playwright UI tests against fixture data
just test-ui
```

Tests use temporary SQLite databases and generated keys; nothing outside the
repository is touched. Conventions for changes live in
[docs/quality.md](docs/quality.md).

## Data Sources

- **Observations**: [MADIS METAR](https://madis.ncep.noaa.gov/madis_metar.shtml) via [Aviation Weather API](https://aviationweather.gov/data/api/)
- **Forecasts**: [NOAA Graphical Forecasts](https://graphical.weather.gov/xml/rest.php)

Data is updated hourly by NOAA; the daemon respects this by fetching once per hour.

## Why This Architecture?

- No remote database needed - just a file server, cheap to run
- Client-side querying via DuckDB-WASM for flexible analysis
- Simple, decoupled components that scale independently
- Immutable data model (snapshots over time)

## License

MIT
