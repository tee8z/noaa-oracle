# Oracle SQLite + S3 Migration Plan

This document outlines the migration from DuckDB to SQLite for transactional data, adding S3 storage for parquet archival, and Litestream for SQLite backup.

## Overview

### Current State
- All event/entry data stored in DuckDB (`events.db3`)
- Weather parquet files stored locally in shared folder
- Daemon writes parquet locally, POSTs to Oracle
- No backups

### Target State
- **SQLite + sqlx**: Events, entries, expected_observations, oracle_metadata (transactional)
- **DuckDB**: Read-only, in-memory for local parquet weather queries (OLAP)
- **S3** (feature flag): Archival storage for parquet files from daemon
- **Litestream**: Continuous SQLite backup to S3
- **Moto**: Local S3 mock for development

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                           DAEMON                                     │
│  Fetches NOAA data → writes parquet                                 │
│                                                                      │
│  Storage destination (one of):                                       │
│  - Network raw_data/ folder (default, current behavior)             │
│  - S3 bucket (feature flag: s3)                                     │
│                                                                      │
│  Then: POST file to Oracle /file/{filename}                         │
└─────────────────────────────────┬───────────────────────────────────┘
                                  │
          ┌───────────────────────┼───────────────────────┐
          ▼                       ▼                       ▼
    Network Folder          S3 Bucket              Oracle API
    raw_data/               (if s3 feature)        POST /file/
    (default)               (archival)                  │
                                  │                     ▼
                                  │           ┌──────────────────────┐
                                  │           │ Oracle writes to     │
                                  │           │ local PVC:           │
                                  │           │ weather_data/        │
                                  │           │                      │
                                  │           │ DuckDB reads from    │
                                  │           │ local PVC for fast   │
                                  │           │ scoring queries      │
                                  │           │                      │
                                  │           │ Cleanup: 30 days     │
                                  │           └──────────────────────┘
                                  │
                                  ▼
                    Historical queries / disaster recovery

┌─────────────────────────────────────────────────────────────────────┐
│                           ORACLE                                     │
│  ┌──────────────────┐    ┌──────────────────────────────────────┐   │
│  │   SQLite + sqlx  │    │  DuckDB (in-memory, read-only)       │   │
│  │  - oracle.db     │    │  - Reads LOCAL parquet files         │   │
│  │  - events        │    │  - Weather analytics for scoring     │   │
│  │  - entries       │    └──────────────────────────────────────┘   │
│  │  - observations  │                                               │
│  │  - metadata      │    ┌──────────────────────────────────────┐   │
│  │  - weather_cache │    │  Litestream sidecar                  │   │
│  │                  │    │  oracle.db → S3 continuous backup    │   │
│  │  DatabaseWriter  │    └──────────────────────────────────────┘   │
│  │  (channel-based) │                                               │
│  └──────────────────┘                                               │
└─────────────────────────────────────────────────────────────────────┘
```

## S3 Buckets

| Bucket | Purpose |
|--------|---------|
| `noaa-oracle-weather` | Parquet files archival - daemon writes (feature flag) |
| `noaa-oracle-backups` | Litestream SQLite backups |

## Storage Strategy

**Daemon storage (one of):**
- **Network folder** (default): Writes to configured `raw_data/` path
- **S3** (feature flag): Writes to `s3://noaa-oracle-weather/`

**Oracle local PVC:**
- Receives parquet via POST from daemon
- Writes to local `weather_data/` folder on PVC
- DuckDB reads from local PVC (fast, ~1ms)
- Background cleanup removes files older than 30 days
- Small PVC (~2GB) sufficient

**S3 archival (when enabled):**
- Long-term storage for historical analysis
- Disaster recovery source
- Not read during normal Oracle operations
- ~$0.02/GB/month

## Feature Flags

### Daemon: `s3` feature
```toml
# crates/daemon/Cargo.toml
[features]
default = []
s3 = ["aws-sdk-s3", "aws-config"]
```

- **Without `s3`**: Writes locally + POSTs to Oracle (current behavior)
- **With `s3`**: Writes locally + uploads to S3 + POSTs to Oracle

### Oracle: `s3` feature (future)
```toml
# crates/oracle/Cargo.toml  
[features]
default = []
s3 = ["aws-sdk-s3", "aws-config"]
```

- **Without `s3`**: Reads parquet from local `weather_data/` folder
- **With `s3`**: Could read from S3 via httpfs (not needed for MVP)

---

## Phase 1: Infrastructure (flake.nix + configs)

### Files to create/modify

**flake.nix additions:**
```nix
# Python environment with moto
moto-env = pkgs.python3.withPackages (ps: with ps; [
  moto flask flask-cors werkzeug boto3
]);

# Add to devShell buildInputs:
moto-env
pkgs.awscli2
pkgs.litestream
```

**config/litestream.yml** (local dev with moto):
```yaml
dbs:
  - path: ./data/oracle.db
    replicas:
      - type: s3
        bucket: noaa-oracle-backups
        path: oracle.db
        region: us-west-2
        endpoint: http://localhost:4566
        force-path-style: true
        sync-interval: 10s
        retention: 24h
        snapshot-interval: 1h

access-key-id: test
secret-access-key: test
```

**config/litestream.production.yml**:
```yaml
dbs:
  - path: /data/oracle.db
    replicas:
      - type: s3
        bucket: noaa-oracle-backups
        path: oracle.db
        region: us-west-2
        sync-interval: 10s
        retention: 168h  # 7 days
        snapshot-interval: 1h

# Credentials from environment: AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY
```

**scripts/start-moto.sh**:
```bash
#!/usr/bin/env bash
set -e

export AWS_ACCESS_KEY_ID=test
export AWS_SECRET_ACCESS_KEY=test
export AWS_DEFAULT_REGION=us-west-2

echo "Starting Moto S3 server..."
moto_server -p 4566 &
sleep 2

echo "Creating S3 buckets..."
aws --endpoint-url=http://localhost:4566 s3 mb s3://noaa-oracle-weather
aws --endpoint-url=http://localhost:4566 s3 mb s3://noaa-oracle-backups

echo "Moto S3 ready on http://localhost:4566"
```

---

## Phase 2: Daemon S3 Storage (Feature Flag)

### Files to modify

**crates/daemon/Cargo.toml:**
```toml
[features]
default = []
s3 = ["aws-sdk-s3", "aws-config"]

[dependencies]
# Existing deps...

[dependencies.aws-sdk-s3]
version = "1.65"
optional = true

[dependencies.aws-config]
version = "1.5"
optional = true
```

**crates/daemon/src/s3_storage.rs** (new file):
```rust
#[cfg(feature = "s3")]
use aws_sdk_s3::Client;
use std::path::Path;

#[cfg(feature = "s3")]
pub struct S3Storage {
    client: Client,
    bucket: String,
}

#[cfg(feature = "s3")]
impl S3Storage {
    pub async fn new(bucket: String, endpoint: Option<String>) -> Result<Self, anyhow::Error> {
        let mut config_loader = aws_config::from_env();
        if let Some(endpoint) = endpoint {
            config_loader = config_loader.endpoint_url(endpoint);
        }
        let config = config_loader.load().await;
        let client = Client::new(&config);
        Ok(Self { client, bucket })
    }

    pub async fn upload_file(&self, local_path: &Path, s3_key: &str) -> Result<(), anyhow::Error> {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(local_path).await?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(s3_key)
            .body(body)
            .send()
            .await?;
        Ok(())
    }
}
```

**crates/daemon/src/parquet_handler.rs** (modify):
```rust
// After generating parquet data:

// 1. Write to storage destination (one of):
#[cfg(feature = "s3")]
if let Some(s3) = &self.s3_storage {
    // S3 mode: write to S3 bucket
    let s3_key = format!("weather_data/{}/{}", date, filename);
    s3.upload_file(&local_path, &s3_key).await?;
} else {
    // Default: write to network folder (raw_data/)
    self.write_to_network_folder(&local_path, &filename).await?;
}

#[cfg(not(feature = "s3"))]
{
    // Default: write to network folder (raw_data/)
    self.write_to_network_folder(&local_path, &filename).await?;
}

// 2. POST to Oracle (always runs - Oracle saves to its own PVC)
self.upload_to_oracle(&local_path, &filename).await?;
```

**crates/daemon/src/utils.rs** (add config):
```rust
#[derive(Parser, Clone, Debug)]
pub struct Cli {
    // Existing fields...

    /// S3 bucket for archival storage (requires --features s3)
    #[arg(long, env = "NOAA_DAEMON_S3_BUCKET")]
    pub s3_bucket: Option<String>,

    /// S3 endpoint URL (for moto/localstack)
    #[arg(long, env = "NOAA_DAEMON_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,
}
```

---

## Phase 3: Oracle SQLite Migration

### Files to create

**crates/oracle/migrations/001_initial_schema.sql:**
```sql
-- Oracle metadata (singleton)
CREATE TABLE oracle_metadata (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    pubkey BLOB NOT NULL,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Events
CREATE TABLE events (
    id TEXT PRIMARY KEY,
    total_allowed_entries INTEGER NOT NULL,
    number_of_places_win INTEGER NOT NULL,
    number_of_values_per_entry INTEGER NOT NULL,
    signing_date TEXT NOT NULL,
    start_observation_date TEXT NOT NULL,
    end_observation_date TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    nonce BLOB NOT NULL,
    event_announcement BLOB NOT NULL,
    locations TEXT NOT NULL,  -- JSON array: ["KORD", "KJFK"]
    coordinator_pubkey TEXT,
    attestation_signature BLOB
);

-- Event entries
CREATE TABLE events_entries (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    score INTEGER NOT NULL DEFAULT 0,
    base_score INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Entry predictions
CREATE TABLE expected_observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_id TEXT NOT NULL REFERENCES events_entries(id) ON DELETE CASCADE,
    station TEXT NOT NULL,
    temp_low TEXT CHECK (temp_low IN ('over', 'par', 'under')),
    temp_high TEXT CHECK (temp_high IN ('over', 'par', 'under')),
    wind_speed TEXT CHECK (wind_speed IN ('over', 'par', 'under'))
);

-- Weather cache for active events (cleaned up after signing)
CREATE TABLE weather_cache (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    station_id TEXT NOT NULL,
    observed_json TEXT,
    forecasted_json TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Indexes
CREATE INDEX idx_entries_event ON events_entries(event_id);
CREATE INDEX idx_observations_entry ON expected_observations(entry_id);
CREATE INDEX idx_weather_event ON weather_cache(event_id);
CREATE INDEX idx_events_attestation ON events(attestation_signature) WHERE attestation_signature IS NULL;
```

**crates/oracle/src/db/database.rs** (new file):
```rust
use anyhow::{Context, Result};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions},
};
use std::{future::Future, str::FromStr, time::Duration};
use tokio::sync::{mpsc, oneshot};
use tracing::info;

type WriteOperation = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

/// Serializes all write operations through a single channel
/// Required for SQLite single-writer model and Litestream compatibility
#[derive(Debug)]
pub struct DatabaseWriter {
    write_tx: mpsc::UnboundedSender<WriteOperation>,
    _handle: tokio::task::JoinHandle<()>,
}

impl Default for DatabaseWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl DatabaseWriter {
    pub fn new() -> Self {
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<WriteOperation>();

        let handle = tokio::spawn(async move {
            while let Some(future) = write_rx.recv().await {
                future.await;
            }
        });

        Self {
            write_tx,
            _handle: handle,
        }
    }

    pub async fn execute<T, F, Fut>(&self, pool: SqlitePool, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(SqlitePool) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel::<Result<T>>();

        let write_op = Box::pin(async move {
            let result = operation(pool).await;
            let _ = result_tx.send(result);
        });

        self.write_tx
            .send(write_op)
            .map_err(|_| anyhow::anyhow!("Database writer channel closed"))?;

        result_rx
            .await
            .map_err(|_| anyhow::anyhow!("Failed to receive write result"))?
    }
}

#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
    writer: std::sync::Arc<DatabaseWriter>,
}

impl Database {
    pub async fn new(path: &str) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let options = SqliteConnectOptions::from_str(&format!("sqlite:{}", path))?
            .create_if_missing(true)
            .pragma("journal_mode", "WAL")
            .pragma("synchronous", "NORMAL")
            .pragma("busy_timeout", "5000")
            .pragma("cache_size", "-64000")  // 64MB
            .pragma("foreign_keys", "ON")
            .pragma("temp_store", "MEMORY");

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(options)
            .await
            .context("Failed to create database connection pool")?;

        let db = Self {
            pool,
            writer: std::sync::Arc::new(DatabaseWriter::new()),
        };

        db.run_migrations().await?;

        info!("Database initialized at: {}", path);

        Ok(db)
    }

    async fn run_migrations(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("Failed to run database migrations")?;
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn writer(&self) -> &DatabaseWriter {
        &self.writer
    }
}
```

### Files to modify

**crates/oracle/Cargo.toml:**
```toml
[dependencies]
# Keep duckdb for weather parquet reads
duckdb = "1.4"

# Add sqlx for SQLite
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite"] }

# Keep existing deps...
```

**crates/oracle/src/db/mod.rs:**
```rust
pub mod database;
pub mod event_data;
pub mod event_db_migrations;  // Keep for reference, will be removed
pub mod outcome_generator;
pub mod weather_data;

pub use database::*;
pub use event_data::*;
// ... rest of exports
```

**crates/oracle/src/db/event_data.rs:**
- Rewrite all methods to use sqlx instead of duckdb
- Use `Database` struct with `writer.execute()` for writes
- Use `pool` directly for reads
- Store locations as JSON string, parse on read

Example query migration:
```rust
// Before (DuckDB)
pub async fn get_stored_public_key(&self) -> Result<XOnlyPublicKey, duckdb::Error> {
    let conn = self.new_readonly_connection_retry().await?;
    let mut stmt = conn.prepare("SELECT pubkey FROM oracle_metadata")?;
    let key: Vec<u8> = stmt.query_row([], |row| row.get(0))?;
    // ...
}

// After (sqlx)
pub async fn get_stored_public_key(&self) -> Result<XOnlyPublicKey, anyhow::Error> {
    let row = sqlx::query!("SELECT pubkey FROM oracle_metadata WHERE id = 1")
        .fetch_optional(self.db.pool())
        .await?
        .ok_or_else(|| anyhow::anyhow!("No oracle metadata found"))?;
    
    XOnlyPublicKey::from_slice(&row.pubkey)
        .map_err(|e| anyhow::anyhow!("Invalid pubkey: {}", e))
}
```

---

## Phase 4: Litestream Configuration

### Helm chart additions

**deploy/helm/noaa-oracle/templates/configmap.yaml:**
```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ include "noaa-oracle.fullname" . }}-litestream
data:
  litestream.yml: |
    dbs:
      - path: /data/oracle.db
        replicas:
          - type: s3
            bucket: {{ .Values.litestream.bucket }}
            path: oracle.db
            region: {{ .Values.litestream.region }}
            {{- if .Values.litestream.endpoint }}
            endpoint: {{ .Values.litestream.endpoint }}
            force-path-style: true
            {{- end }}
            sync-interval: {{ .Values.litestream.syncInterval | default "10s" }}
            retention: {{ .Values.litestream.retention | default "168h" }}
            snapshot-interval: {{ .Values.litestream.snapshotInterval | default "1h" }}
```

**deploy/helm/noaa-oracle/templates/deployment.yaml** (add sidecar):
```yaml
containers:
  - name: oracle
    # ... existing oracle container

  - name: litestream
    image: litestream/litestream:0.3
    args:
      - replicate
      - -config
      - /etc/litestream/litestream.yml
    volumeMounts:
      - name: data
        mountPath: /data
      - name: litestream-config
        mountPath: /etc/litestream
    env:
      - name: AWS_ACCESS_KEY_ID
        valueFrom:
          secretKeyRef:
            name: {{ include "noaa-oracle.fullname" . }}-s3
            key: access-key-id
      - name: AWS_SECRET_ACCESS_KEY
        valueFrom:
          secretKeyRef:
            name: {{ include "noaa-oracle.fullname" . }}-s3
            key: secret-access-key
```

**deploy/helm/noaa-oracle/values.yaml:**
```yaml
litestream:
  enabled: true
  bucket: noaa-oracle-backups
  region: us-west-2
  endpoint: ""  # Set to moto endpoint for local dev
  syncInterval: "10s"
  retention: "168h"
  snapshotInterval: "1h"

s3:
  enabled: false  # Feature flag for daemon S3 uploads
  bucket: noaa-oracle-weather
  region: us-west-2
  endpoint: ""
```

---

## Phase 5: sqlx Offline Mode

Generate query metadata for compile-time checking:

```bash
# Set DATABASE_URL for sqlx
export DATABASE_URL="sqlite:./data/oracle.db"

# Run migrations to create schema
cargo sqlx database create
cargo sqlx migrate run

# Generate .sqlx folder with query metadata
cargo sqlx prepare --workspace

# Commit .sqlx folder to git
git add .sqlx/
```

**crates/oracle/Cargo.toml:**
```toml
[package.metadata.sqlx]
offline = true
```

---

## Migration Checklist

### Phase 1: Infrastructure
- [ ] Add moto-env, litestream, awscli2 to flake.nix
- [ ] Create config/litestream.yml
- [ ] Create config/litestream.production.yml
- [ ] Create scripts/start-moto.sh

### Phase 2: Daemon S3 (Feature Flag)
- [ ] Add s3 feature to crates/daemon/Cargo.toml
- [ ] Create crates/daemon/src/s3_storage.rs
- [ ] Modify parquet_handler.rs to upload to S3 when feature enabled
- [ ] Add S3 config options to CLI

### Phase 3: Oracle SQLite
- [ ] Add sqlx dependency to crates/oracle/Cargo.toml
- [ ] Create crates/oracle/migrations/001_initial_schema.sql
- [ ] Create crates/oracle/src/db/database.rs (DatabaseWriter)
- [ ] Migrate event_data.rs from DuckDB to sqlx
- [ ] Update startup.rs to use new Database
- [ ] Keep DuckDB for weather_data.rs (parquet reads)

### Phase 4: Litestream
- [ ] Create deploy/helm/noaa-oracle/templates/configmap.yaml (litestream)
- [ ] Add litestream sidecar to deployment.yaml
- [ ] Add litestream values to values.yaml

### Phase 5: sqlx Offline
- [ ] Generate .sqlx/ query metadata
- [ ] Add to git, update CI

### Phase 6: Oracle Parquet Cleanup
- [ ] Add background task to delete parquet files older than 30 days
- [ ] Run cleanup on startup and daily thereafter
- [ ] Log cleanup actions

### Phase 7: Testing
- [ ] Test with moto locally
- [ ] Test daemon S3 uploads
- [ ] Test Oracle SQLite operations
- [ ] Test Litestream backup/restore
- [ ] Test without S3 feature (default behavior)
- [ ] Test 30-day parquet cleanup

---

## Rollback Plan

If issues occur:
1. Disable `s3` feature flag on daemon (reverts to shared folder only)
2. Keep DuckDB event database as fallback (don't delete events.db3)
3. Litestream can restore SQLite from S3 backup

---

## References

- [Litestream Documentation](https://litestream.io/guides/s3/)
- [sqlx Documentation](https://github.com/launchbadge/sqlx)
- [DuckDB S3 Support](https://duckdb.org/docs/stable/extensions/httpfs/s3api)
- [Keymeld Database Pattern](/home/teebz/repos/keymeld/crates/keymeld-gateway/src/database.rs)

---

Created: 2025-01-11
Related: Infrastructure deployment plan
