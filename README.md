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

**Event storage:** the oracle keeps DLC events in one SQLite database. A
single writer task owns the only writable connection and runs commands from
a bounded queue; HTTP handlers read through a read-only pool and receive a
reply only after a commit. A full queue answers HTTP 503 so the coordinator
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

The oracle crate links against the DuckDB C library. Download the library matching the `duckdb` crate version in `crates/oracle/Cargo.toml` from [DuckDB releases](https://github.com/duckdb/duckdb/releases) (e.g., `libduckdb-linux-amd64.zip`) and point to it:

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

```toml
[oracle]
host = "127.0.0.1"
port = "9800"
log_level = "info"

# Path to weather data (parquet files)
data_dir = "/var/lib/noaa-oracle/weather"

# Directory holding events.sqlite
event_db = "/var/lib/noaa-oracle/events"

# Path to UI files
ui_dir = "/usr/share/noaa-oracle/ui"

# Oracle private key for DLC attestation
private_key_path = "/etc/noaa-oracle/keys/oracle.pem"

# Seconds to finish requests and accepted writes after SIGTERM
shutdown_timeout = 25
```

`GET /health` reports readiness (writer available and a database read succeeds) and turns 503 during shutdown; `GET /healthy` reports HTTP liveness only.

### Daemon Configuration

```toml
[daemon]
log_level = "info"

# Where to store downloaded parquet files
data_path = "/var/lib/noaa-oracle/data"

# Oracle endpoint to push files to
oracle_url = "http://localhost:9800"

# Fetch interval in seconds (default: 3600 = 1 hour)
fetch_interval = 3600
```

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
            };
            daemon = {
              enable = true;
              fetchInterval = 3600;
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
