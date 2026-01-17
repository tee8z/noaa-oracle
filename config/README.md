# Configuration

This directory contains example configuration files for the NOAA Oracle services.

## Architecture Overview

```
┌─────────────────┐         ┌─────────────────┐
│   NOAA APIs     │         │  Other Oracle   │
│ (weather.gov)   │         │   (optional)    │
└────────┬────────┘         └────────┬────────┘
         │                           │
         ▼                           │
┌─────────────────┐                  │
│     Daemon      │                  │
│  (optional)     │                  │
│                 │                  │
│ Fetches data    │                  │
│ Creates parquet │                  │
│ Uploads to      │                  │
│ oracle          │                  │
└────────┬────────┘                  │
         │                           │
         ▼                           ▼
┌─────────────────────────────────────────────┐
│                   Oracle                     │
│                                              │
│  • Serves weather data (parquet files)      │
│  • Hosts browser UI for data queries        │
│  • Manages DLC prediction events            │
│  • Signs attestations for dlctix contracts  │
│                                              │
│  Can run standalone with pre-existing data! │
└──────────────────────────────────────────────┘
```

## Two Independent Services

### Oracle (required)
The Oracle is the main service. It can run **without** the daemon by using:
- Pre-downloaded weather data
- Data synced from another oracle
- Historical datasets

```bash
# Run oracle with existing data
oracle --data-dir /path/to/weather_data
```

### Daemon (optional)
The Daemon fetches fresh data from NOAA and uploads to an Oracle.
Only needed if you want live/current weather data.

```bash
# Run daemon pointing at your oracle
daemon --base-url http://your-oracle:9800
```

## Quick Start

```bash
# Copy example configs
cp config/oracle.example.toml oracle.toml
cp config/daemon.example.toml daemon.toml  # optional

# Edit as needed, then run
just run-oracle

# Optionally, in another terminal:
just run-daemon
```

## Configuration Search Order

Both services search for configuration in this order:

1. `--config` CLI flag (explicit path)
2. Environment variable (`NOAA_ORACLE_CONFIG` or `NOAA_DAEMON_CONFIG`)
3. `./oracle.toml` or `./daemon.toml` (current directory)
4. `$XDG_CONFIG_HOME/noaa-oracle/*.toml` (usually `~/.config/noaa-oracle/`)
5. `/etc/noaa-oracle/*.toml` (system-wide)

## Environment Variables

All config options can be set via environment variables:

### Oracle
```bash
export NOAA_ORACLE_LEVEL=info
export NOAA_ORACLE_HOST=0.0.0.0
export NOAA_ORACLE_PORT=9800
export NOAA_ORACLE_DATA_DIR=/path/to/weather_data
export NOAA_ORACLE_EVENT_DB=/path/to/events
export NOAA_ORACLE_UI_DIR=/usr/share/noaa-oracle/static
export NOAA_ORACLE_PRIVATE_KEY_PATH=/etc/noaa-oracle/oracle.pem
```

### Daemon
```bash
export NOAA_DAEMON_LEVEL=info
export NOAA_DAEMON_BASE_URL=http://localhost:9800
export NOAA_DAEMON_DATA_DIR=/var/cache/noaa-oracle
export NOAA_DAEMON_SLEEP_INTERVAL=3600
```

## Deployment Scenarios

### Scenario 1: Full Stack (Oracle + Daemon)
Run both services for live weather data:
```bash
just run-oracle &
just run-daemon &
```

### Scenario 2: Oracle Only (Pre-existing Data)
Use existing weather data without fetching new data:
```bash
# Download or copy weather_data from somewhere
oracle --data-dir /path/to/existing/weather_data
```

### Scenario 3: Multiple Oracles, Single Daemon
Run one daemon feeding multiple oracle instances:
```bash
# On each oracle server
oracle --port 9800

# On daemon server, configure to upload to primary oracle
daemon --base-url http://primary-oracle:9800

# Sync weather_data between oracles via rsync/s3/etc
```

## NixOS Module

When using the NixOS module, configuration is handled via module options:

```nix
services.noaa-oracle = {
  enable = true;
  
  oracle = {
    enable = true;
    host = "0.0.0.0";
    port = 9800;
    dataDir = "/var/lib/noaa-oracle/weather";
  };
  
  daemon = {
    enable = true;  # Set to false for oracle-only deployment
    oracleUrl = "http://localhost:9800";
  };
};
```
