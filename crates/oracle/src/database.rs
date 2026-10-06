//! Event storage on SQLite.
//!
//! One writable connection lives in [`DatabaseWriter`] and runs commands from
//! a bounded queue in order. HTTP handlers and the ETL hold a cloneable
//! [`Database`] that can read through a query-only pool and submit write
//! commands, but never touch a writable connection. A write replies only
//! after its transaction commits; a full or closed queue rejects the write
//! before admission, and a lost reply after admission is an unknown outcome
//! that the caller must not retry blindly.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use dlctix::{
    EventLockingConditions,
    musig2::secp256k1::XOnlyPublicKey,
    secp::{MaybeScalar, Point},
};
use futures::future::BoxFuture;
use log::info;
use serde::de::DeserializeOwned;
use sqlx::{
    AssertSqlSafe, Connection, Row, SqliteConnection, SqlitePool,
    migrate::Migrator,
    sqlite::{
        SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
    },
};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    events::{
        EXPIRY_AFTER_SIGNING, Entry, EventCounts, EventListQuery, EventRecord, EventStatus,
        NewEvent, SettlementBlock, ValueOptions,
    },
    lines::{Line, LineLevel},
    scoring::{Pick, ScoringRules},
    signing::EventNonce,
    sources::Reading,
};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// File name inside the event directory. Litestream and the Helm chart refer
/// to this path, so keep it stable.
pub const DATABASE_FILE: &str = "events.sqlite";
pub const WRITE_QUEUE_CAPACITY: usize = 64;
const READER_CONNECTIONS: u32 = 4;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const ORACLE_NAME: &str = "4casttruth";

type Command = Box<dyn for<'a> FnOnce(&'a mut SqliteConnection) -> BoxFuture<'a, ()> + Send>;

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The queue was full or closed; nothing was written.
    #[error("database write was not accepted")]
    Unavailable,
    /// The write was accepted but the reply was lost; it may have committed.
    #[error("database write was accepted but its outcome is unknown")]
    OutcomeUnknown,
    #[error("database write failed: {0}")]
    Database(Box<sqlx::Error>),
}

impl From<sqlx::Error> for WriteError {
    fn from(error: sqlx::Error) -> Self {
        WriteError::Database(Box::new(error))
    }
}

/// Read access and write admission. Clones share one queue and one writer.
#[derive(Clone)]
pub struct Database {
    readers: SqlitePool,
    commands: mpsc::Sender<Command>,
    ready: Arc<AtomicBool>,
}

/// Owns the only writable connection. The process must supervise `run` and
/// cancel it only after every producer of writes has finished.
pub struct DatabaseWriter {
    connection: SqliteConnection,
    readers: SqlitePool,
    commands: mpsc::Receiver<Command>,
    ready: Arc<AtomicBool>,
}

impl Database {
    /// Atomically claim a verified request across restarts and serving processes.
    pub async fn claim_auth_event(
        &self,
        id: String,
        expires_at: i64,
        capacity: i64,
    ) -> Result<bool, WriteError> {
        self.write(move |connection| {
            Box::pin(async move {
                let mut tx = connection.begin().await?;
                sqlx::query("DELETE FROM nip98_consumed_events WHERE expires_at < ?")
                    .bind(OffsetDateTime::now_utc().unix_timestamp())
                    .execute(&mut *tx)
                    .await?;
                // The body read, write queue, or lock acquisition can outlive the
                // extractor's freshness check. Recheck after taking the write lock.
                if expires_at < OffsetDateTime::now_utc().unix_timestamp() {
                    return Err(sqlx::Error::Protocol(
                        "Authentication proof expired before storage".into(),
                    ));
                }
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM nip98_consumed_events WHERE event_id = ?)",
                )
                .bind(&id)
                .fetch_one(&mut *tx)
                .await?;
                if exists {
                    return Ok(false);
                }
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nip98_consumed_events")
                    .fetch_one(&mut *tx)
                    .await?;
                if count >= capacity {
                    return Err(sqlx::Error::Protocol(
                        "Authentication replay capacity reached".into(),
                    ));
                }
                sqlx::query(
                    "INSERT INTO nip98_consumed_events (event_id, expires_at) VALUES (?, ?)",
                )
                .bind(id)
                .bind(expires_at)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(true)
            })
        })
        .await
    }

    /// Opens or creates `<directory>/events.sqlite`, applies migrations, and
    /// returns the shared handle with its writer.
    pub async fn open(directory: &Path) -> Result<(Self, DatabaseWriter)> {
        Self::open_with_capacity(directory, WRITE_QUEUE_CAPACITY).await
    }

    pub(crate) async fn open_with_capacity(
        directory: &Path,
        capacity: usize,
    ) -> Result<(Self, DatabaseWriter)> {
        ensure!(
            capacity > 0,
            "database write queue capacity must be positive"
        );
        tokio::fs::create_dir_all(directory)
            .await
            .with_context(|| format!("create event directory {}", directory.display()))?;
        let path: PathBuf = directory.join(DATABASE_FILE);
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .context("open writable SQLite connection")?;
        MIGRATOR
            .run_direct(None, &mut connection, false)
            .await
            .context("apply SQLite migrations")?;
        let readers = SqlitePoolOptions::new()
            .max_connections(READER_CONNECTIONS)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .read_only(true)
                    .pragma("query_only", "ON")
                    .foreign_keys(true)
                    .busy_timeout(BUSY_TIMEOUT),
            )
            .await
            .context("open SQLite reader pool")?;
        let (commands, receiver) = mpsc::channel(capacity);
        let ready = Arc::new(AtomicBool::new(true));
        let writer = DatabaseWriter {
            connection,
            readers: readers.clone(),
            commands: receiver,
            ready: ready.clone(),
        };
        info!("SQLite event database ready at {}", path.display());
        Ok((
            Self {
                readers,
                commands,
                ready,
            },
            writer,
        ))
    }

    /// Stops reporting readiness before HTTP drains so load balancers stop
    /// routing new requests here.
    pub fn stop_readiness(&self) {
        self.ready.store(false, Ordering::Release);
    }

    pub fn is_writer_available(&self) -> bool {
        self.ready.load(Ordering::Acquire) && !self.commands.is_closed()
    }

    /// Readiness: the writer accepts commands and a read succeeds.
    pub async fn is_ready(&self) -> bool {
        self.is_writer_available()
            && sqlx::query_scalar::<_, i64>("SELECT 1")
                .fetch_one(&self.readers)
                .await
                .is_ok()
    }

    /// Request-path writes reject immediately when the queue is full.
    async fn write<T, F>(&self, operation: F) -> Result<T, WriteError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut SqliteConnection) -> BoxFuture<'a, Result<T, sqlx::Error>>
            + Send
            + 'static,
    {
        let (command, response) = command(operation);
        self.commands
            .try_send(command)
            .map_err(|_| WriteError::Unavailable)?;
        response
            .await
            .map_err(|_| WriteError::OutcomeUnknown)?
            .map_err(WriteError::from)
    }

    /// Background producers (the ETL) are already bounded, so they wait for
    /// queue capacity instead of dropping work they computed.
    async fn write_waiting<T, F>(&self, operation: F) -> Result<T, WriteError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut SqliteConnection) -> BoxFuture<'a, Result<T, sqlx::Error>>
            + Send
            + 'static,
    {
        let (command, response) = command(operation);
        self.commands
            .send(command)
            .await
            .map_err(|_| WriteError::Unavailable)?;
        response
            .await
            .map_err(|_| WriteError::OutcomeUnknown)?
            .map_err(WriteError::from)
    }

    pub async fn add_oracle_metadata(&self, pubkey: XOnlyPublicKey) -> Result<(), WriteError> {
        let pubkey_bytes = pubkey.serialize().to_vec();
        self.write(move |connection| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO oracle_metadata (pubkey, name) VALUES (?, ?)
                     ON CONFLICT(pubkey) DO NOTHING",
                )
                .bind(pubkey_bytes)
                .bind(ORACLE_NAME)
                .execute(connection)
                .await?;
                Ok(())
            })
        })
        .await
    }

    pub async fn get_stored_public_key(&self) -> Result<Option<XOnlyPublicKey>, sqlx::Error> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT pubkey FROM oracle_metadata LIMIT 1")
            .fetch_optional(&self.readers)
            .await?;
        row.map(|(bytes,)| {
            let bytes: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|error| decode_error("pubkey", error))?;
            XOnlyPublicKey::from_byte_array(bytes).map_err(|error| decode_error("pubkey", error))
        })
        .transpose()
    }

    /// Stores a new event with its lines, together.
    pub async fn add_event(&self, event: &NewEvent) -> Result<(), WriteError> {
        let row = EventInsert::encode(event)?;
        let lines = event.lines.clone();
        self.write(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                sqlx::query(
                    "INSERT INTO events (
                        id, source, total_allowed_entries, number_of_places_win,
                        number_of_values_per_entry, signing_date,
                        start_observation_date, end_observation_date,
                        nonce_salt, nonce_point, event_announcement,
                        locations, metrics, coordinator_pubkey, unlisted, scoring_rules
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(row.id.clone())
                .bind(row.source)
                .bind(row.total_allowed_entries)
                .bind(row.number_of_places_win)
                .bind(row.number_of_values_per_entry)
                .bind(row.signing_date)
                .bind(row.start_observation_date)
                .bind(row.end_observation_date)
                .bind(row.nonce_salt)
                .bind(row.nonce_point)
                .bind(row.event_announcement)
                .bind(row.locations)
                .bind(row.metrics)
                .bind(row.coordinator_pubkey)
                .bind(row.unlisted)
                .bind(row.scoring_rules)
                .execute(&mut *transaction)
                .await?;
                for line in &lines {
                    sqlx::query(
                        "INSERT INTO event_lines (
                            event_id, target, metric, lower, upper, level, window_hours,
                            windows, over, par, under, first_window, last_window, fitted_at
                        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(&row.id)
                    .bind(&line.target)
                    .bind(&line.metric)
                    .bind(line.lower)
                    .bind(line.upper)
                    .bind(line.level.as_str())
                    .bind(line.window_hours)
                    .bind(line.windows)
                    .bind(line.over)
                    .bind(line.par)
                    .bind(line.under)
                    .bind(line.first_window.unix_timestamp())
                    .bind(line.last_window.unix_timestamp())
                    .bind(line.fitted_at.unix_timestamp())
                    .execute(&mut *transaction)
                    .await?;
                }
                transaction.commit().await
            })
        })
        .await
    }

    /// Lines of each of `event_ids` that has them, in target and metric order.
    pub async fn event_lines(
        &self,
        event_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<Line>>, sqlx::Error> {
        if event_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids = serde_json::to_string(&event_ids.iter().map(Uuid::to_string).collect::<Vec<_>>())
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let rows = sqlx::query(
            "SELECT event_id, target, metric, lower, upper, level, window_hours, windows,
                    over, par, under, first_window, last_window, fitted_at
             FROM event_lines WHERE event_id IN (SELECT value FROM json_each(?))
             ORDER BY event_id, target, metric",
        )
        .bind(ids)
        .fetch_all(&self.readers)
        .await?;
        let mut lines: HashMap<Uuid, Vec<Line>> = HashMap::new();
        for row in &rows {
            lines
                .entry(uuid_column(row, "event_id")?)
                .or_default()
                .push(line_from_row(row)?);
        }
        Ok(lines)
    }

    /// Stores a full set of entries. Returns `false`, writing nothing, when
    /// the event already has entries: submission is all at once, and the
    /// check runs inside the write so concurrent submissions cannot both win.
    pub async fn add_event_entries(
        &self,
        event_id: Uuid,
        entries: Vec<Entry>,
    ) -> Result<bool, WriteError> {
        self.write(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                let existing: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM events_entries WHERE event_id = ?")
                        .bind(event_id.to_string())
                        .fetch_one(&mut *transaction)
                        .await?;
                if existing > 0 {
                    return Ok(false);
                }
                for entry in &entries {
                    sqlx::query("INSERT INTO events_entries (id, event_id) VALUES (?, ?)")
                        .bind(entry.id.to_string())
                        .bind(event_id.to_string())
                        .execute(&mut *transaction)
                        .await?;
                    for (position, pick) in entry.picks.iter().enumerate() {
                        sqlx::query(
                            "INSERT INTO entry_picks (entry_id, position, target, metric, prediction)
                             VALUES (?, ?, ?, ?, ?)",
                        )
                        .bind(entry.id.to_string())
                        .bind(i64::try_from(position).unwrap_or(i64::MAX))
                        .bind(&pick.target)
                        .bind(&pick.metric)
                        .bind(pick.prediction.as_str())
                        .execute(&mut *transaction)
                        .await?;
                    }
                }
                transaction.commit().await?;
                Ok(true)
            })
        })
        .await
    }

    pub async fn get_event(&self, id: Uuid) -> Result<Option<EventRecord>, sqlx::Error> {
        // Only constant SQL is interpolated; the id is bound.
        let sql = format!("{EVENT_SELECT} WHERE e.id = ? GROUP BY e.id");
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(id.to_string())
            .fetch_optional(&self.readers)
            .await?;
        row.as_ref().map(event_from_row).transpose()
    }

    /// Newest events first. `ids`, when not empty, restricts the list.
    pub async fn list_events(
        &self,
        ids: &[Uuid],
        limit: usize,
    ) -> Result<Vec<EventRecord>, sqlx::Error> {
        let filter = if ids.is_empty() {
            String::new()
        } else {
            format!(" WHERE e.id IN ({})", vec!["?"; ids.len()].join(","))
        };
        // Only `?` placeholders are interpolated; ids and the limit are bound.
        let sql = format!("{EVENT_SELECT}{filter} GROUP BY e.id ORDER BY e.id DESC LIMIT ?");
        let mut query = sqlx::query(AssertSqlSafe(sql));
        for id in ids {
            query = query.bind(id.to_string());
        }
        let rows = query
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&self.readers)
            .await?;
        rows.iter().map(event_from_row).collect()
    }

    /// A page of the UI's events list, newest first. Unlisted events and
    /// the status are filtered here, before the limit.
    pub async fn event_page(
        &self,
        query: &EventListQuery,
        now: OffsetDateTime,
    ) -> Result<Vec<EventRecord>, sqlx::Error> {
        let now = now.unix_timestamp();
        let mut conditions = vec![];
        let mut binds = vec![];
        if !query.include_unlisted {
            conditions.push(LISTED);
        }
        if let Some(status) = query.status {
            let (condition, now_binds) = status_condition(status);
            conditions.push(condition);
            binds.extend(std::iter::repeat_n(now, now_binds));
        }
        let before = query.before.map(|id| id.to_string());
        if before.is_some() {
            conditions.push("e.id < ?");
        }
        let filter = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        // Only constant conditions are interpolated; values are bound.
        let sql = format!("{EVENT_SELECT}{filter} GROUP BY e.id ORDER BY e.id DESC LIMIT ?");
        let mut statement = sqlx::query(AssertSqlSafe(sql));
        for value in binds {
            statement = statement.bind(value);
        }
        if let Some(before) = before {
            statement = statement.bind(before);
        }
        let rows = statement
            .bind(i64::try_from(query.limit).unwrap_or(i64::MAX))
            .fetch_all(&self.readers)
            .await?;
        rows.iter().map(event_from_row).collect()
    }

    /// Events by status, with or without unlisted events, and the number of
    /// unlisted events either way.
    pub async fn event_counts(
        &self,
        include_unlisted: bool,
        now: OffsetDateTime,
    ) -> Result<EventCounts, sqlx::Error> {
        let now = now.unix_timestamp();
        let row = sqlx::query(
            "WITH e AS (
                 SELECT attestation, start_observation_date AS start_,
                        end_observation_date AS end_, (? OR unlisted = 0) AS counted, unlisted
                 FROM events)
             SELECT
                 COALESCE(SUM(counted AND attestation IS NULL AND ? < start_), 0) AS live,
                 COALESCE(SUM(counted AND attestation IS NULL AND start_ <= ? AND ? < end_), 0)
                     AS running,
                 COALESCE(SUM(counted AND attestation IS NULL AND end_ <= ?), 0) AS completed,
                 COALESCE(SUM(counted AND attestation IS NOT NULL), 0) AS signed,
                 COALESCE(SUM(unlisted), 0) AS unlisted
             FROM e",
        )
        .bind(include_unlisted)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(now)
        .fetch_one(&self.readers)
        .await?;
        Ok(EventCounts {
            live: count_column(&row, "live")?,
            running: count_column(&row, "running")?,
            completed: count_column(&row, "completed")?,
            signed: count_column(&row, "signed")?,
            unlisted: count_column(&row, "unlisted")?,
        })
    }

    /// Events, listed or not, whose observation window has ended and that
    /// have entries but no attestation yet, with the earliest signing date
    /// among them. Events without entries are never attested, so they are
    /// left out. Those past their DLC expiry can no longer be attested and
    /// are counted apart. Of those still awaiting, the ones whose latest
    /// check found the published observations short of the window, or no
    /// baseline for a scored metric (the code
    /// [`crate::events::BASELINE_UNAVAILABLE`]), are counted again apart,
    /// and so are the latter alone. The earliest signing date among those
    /// not waiting on observations is given too; it includes events without
    /// a baseline, which nothing but the operator can resolve.
    pub async fn awaiting_attestation(
        &self,
        now: OffsetDateTime,
    ) -> Result<AwaitingAttestation, sqlx::Error> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(e.signing_date + ?1 > ?2), 0) AS awaiting,
                COALESCE(SUM(e.signing_date + ?1 <= ?2), 0) AS expired,
                MIN(CASE WHEN e.signing_date + ?1 > ?2 THEN e.signing_date END) AS oldest_signing_date,
                COALESCE(SUM(e.signing_date + ?1 > ?2
                    AND (b.source_coverage IS 1 OR b.code IS 'baseline_unavailable')), 0)
                    AS blocked_on_source_coverage,
                COALESCE(SUM(e.signing_date + ?1 > ?2 AND b.code IS 'baseline_unavailable'), 0)
                    AS unsettleable,
                MIN(CASE WHEN e.signing_date + ?1 > ?2 AND b.source_coverage IS NOT 1
                    THEN e.signing_date END) AS oldest_attestable_signing_date
             FROM events e LEFT JOIN event_settlement_blocks b ON b.event_id = e.id
             WHERE e.attestation IS NULL AND e.end_observation_date <= ?2
               AND EXISTS (SELECT 1 FROM events_entries x WHERE x.event_id = e.id)",
        )
        .bind(EXPIRY_AFTER_SIGNING.whole_seconds())
        .bind(now.unix_timestamp())
        .fetch_one(&self.readers)
        .await?;
        let date = |column: &str| -> Result<Option<OffsetDateTime>, sqlx::Error> {
            let seconds: Option<i64> = row.try_get(column)?;
            seconds
                .map(OffsetDateTime::from_unix_timestamp)
                .transpose()
                .map_err(|error| decode_error(column, error))
        };
        Ok(AwaitingAttestation {
            count: count_column(&row, "awaiting")?,
            expired: count_column(&row, "expired")?,
            oldest_signing_date: date("oldest_signing_date")?,
            blocked_on_source_coverage: count_column(&row, "blocked_on_source_coverage")?,
            unsettleable: count_column(&row, "unsettleable")?,
            oldest_attestable_signing_date: date("oldest_attestable_signing_date")?,
        })
    }

    /// Unsigned events the ETL still has work for. An event past its DLC
    /// expiry is left alone: its contract refunds through the expiry path,
    /// and an event that could never settle would otherwise be read every
    /// pass forever. An event settled without entries has no outcome left to
    /// sign. Which of these a pass reads is the oracle's decision.
    pub async fn events_to_settle(
        &self,
        now: OffsetDateTime,
    ) -> Result<Vec<EventRecord>, sqlx::Error> {
        // Only constant SQL is interpolated.
        let sql = format!(
            "{EVENT_SELECT} WHERE e.attestation IS NULL AND e.settled_without_entries_at IS NULL
             AND e.signing_date + ? > ?
             GROUP BY e.id ORDER BY e.id"
        );
        let rows = sqlx::query(AssertSqlSafe(sql))
            .bind(EXPIRY_AFTER_SIGNING.whole_seconds())
            .bind(now.unix_timestamp())
            .fetch_all(&self.readers)
            .await?;
        rows.iter().map(event_from_row).collect()
    }

    pub async fn event_announcement(
        &self,
        id: Uuid,
    ) -> Result<Option<EventLockingConditions>, sqlx::Error> {
        let row = sqlx::query("SELECT event_announcement FROM events WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.readers)
            .await?;
        row.as_ref()
            .map(|row| json_column(row, "event_announcement"))
            .transpose()
    }

    /// Entries in id order: entry `i` is outcome index `i`.
    pub async fn event_entries(&self, event_id: Uuid) -> Result<Vec<Entry>, sqlx::Error> {
        self.entries(event_id, None).await
    }

    pub async fn event_entry(
        &self,
        event_id: Uuid,
        entry_id: Uuid,
    ) -> Result<Option<Entry>, sqlx::Error> {
        Ok(self.entries(event_id, Some(entry_id)).await?.pop())
    }

    async fn entries(
        &self,
        event_id: Uuid,
        entry_id: Option<Uuid>,
    ) -> Result<Vec<Entry>, sqlx::Error> {
        let entry_filter = entry_id.map(|id| id.to_string());
        let rows = sqlx::query(
            "SELECT id, score, base_score FROM events_entries
             WHERE event_id = ? AND (? IS NULL OR id = ?)
             ORDER BY id",
        )
        .bind(event_id.to_string())
        .bind(&entry_filter)
        .bind(&entry_filter)
        .fetch_all(&self.readers)
        .await?;
        let pick_rows = sqlx::query(
            "SELECT p.entry_id, p.target, p.metric, p.prediction
             FROM entry_picks p
             JOIN events_entries ee ON ee.id = p.entry_id
             WHERE ee.event_id = ? AND (? IS NULL OR ee.id = ?)
             ORDER BY p.entry_id, p.position",
        )
        .bind(event_id.to_string())
        .bind(&entry_filter)
        .bind(&entry_filter)
        .fetch_all(&self.readers)
        .await?;
        let mut picks: HashMap<Uuid, Vec<Pick>> = HashMap::new();
        for row in &pick_rows {
            let prediction: String = row.try_get("prediction")?;
            picks
                .entry(uuid_column(row, "entry_id")?)
                .or_default()
                .push(Pick {
                    target: row.try_get("target")?,
                    metric: row.try_get("metric")?,
                    prediction: ValueOptions::from_storage(&prediction).ok_or_else(|| {
                        decode_error("prediction", format!("unknown prediction {prediction:?}"))
                    })?,
                });
        }
        rows.iter()
            .map(|row| {
                let id = uuid_column(row, "id")?;
                Ok(Entry {
                    id,
                    event_id,
                    picks: picks.remove(&id).unwrap_or_default(),
                    score: row.try_get("score")?,
                    base_score: row.try_get("base_score")?,
                })
            })
            .collect()
    }

    /// Latest failed checks for unsigned events. Signed history has no mutable
    /// settlement state, even if a pre-signing failure was recorded earlier.
    pub async fn settlement_blocks(
        &self,
        event_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, SettlementBlock>, sqlx::Error> {
        if event_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids = serde_json::to_string(&event_ids.iter().map(Uuid::to_string).collect::<Vec<_>>())
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let rows = sqlx::query(
            "SELECT b.event_id, b.code, b.message, b.checked_at
             FROM event_settlement_blocks b JOIN events e ON e.id = b.event_id
             WHERE e.attestation IS NULL AND b.event_id IN (SELECT value FROM json_each(?))",
        )
        .bind(ids)
        .fetch_all(&self.readers)
        .await?;
        rows.iter()
            .map(|row| {
                let timestamp: i64 = row.try_get("checked_at")?;
                let checked_at = OffsetDateTime::from_unix_timestamp(timestamp)
                    .map_err(|error| decode_error("checked_at", error.to_string()))?;
                Ok((
                    uuid_column(row, "event_id")?,
                    SettlementBlock {
                        code: row.try_get("code")?,
                        message: row.try_get("message")?,
                        checked_at,
                    },
                ))
            })
            .collect()
    }

    /// Records a failure through the same single writer as event processing.
    /// A signed event cannot acquire or overwrite a blocked reason.
    /// `source_coverage` marks a failure for published observations that do
    /// not cover the window rather than a fault in the oracle; only metrics
    /// read it.
    pub async fn set_settlement_block(
        &self,
        event_id: Uuid,
        block: SettlementBlock,
        source_coverage: bool,
    ) -> Result<(), WriteError> {
        self.write_waiting(move |connection| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO event_settlement_blocks
                    (event_id, code, message, checked_at, source_coverage)
                 SELECT id, ?, ?, ?, ? FROM events
                 WHERE id = ? AND attestation IS NULL AND settled_without_entries_at IS NULL
                 ON CONFLICT(event_id) DO UPDATE SET code = excluded.code,
                    message = excluded.message, checked_at = excluded.checked_at,
                    source_coverage = excluded.source_coverage",
                )
                .bind(block.code)
                .bind(block.message)
                .bind(block.checked_at.unix_timestamp())
                .bind(source_coverage)
                .bind(event_id.to_string())
                .execute(connection)
                .await?;
                Ok(())
            })
        })
        .await
    }

    /// Closes an unsigned event that has no entries, and so no outcome to
    /// sign, and drops its blocked reason. False when the event has entries,
    /// is signed, or was already settled this way.
    pub async fn settle_without_entries(
        &self,
        event_id: Uuid,
        at: OffsetDateTime,
    ) -> Result<bool, WriteError> {
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
                let event_id = event_id.to_string();
                let result = sqlx::query(
                    "UPDATE events SET settled_without_entries_at = ?, updated_at = unixepoch()
                     WHERE id = ? AND attestation IS NULL AND settled_without_entries_at IS NULL
                       AND NOT EXISTS (SELECT 1 FROM events_entries x WHERE x.event_id = events.id)",
                )
                .bind(at.unix_timestamp())
                .bind(&event_id)
                .execute(&mut *transaction)
                .await?;
                if result.rows_affected() == 0 {
                    transaction.rollback().await?;
                    return Ok(false);
                }
                sqlx::query("DELETE FROM event_settlement_blocks WHERE event_id = ?")
                    .bind(&event_id)
                    .execute(&mut *transaction)
                    .await?;
                transaction.commit().await?;
                Ok(true)
            })
        })
        .await
    }

    /// Takes or renews the lease on `name` for `ttl`. False while another
    /// process holds it; an expired or released lease passes to `holder`.
    pub async fn take_lease(
        &self,
        name: &str,
        holder: &str,
        ttl: std::time::Duration,
    ) -> Result<bool, WriteError> {
        let (name, holder) = (name.to_owned(), holder.to_owned());
        let now = unix_millis();
        let expires_at = now.saturating_add(ttl.as_millis() as i64);
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let result = sqlx::query(
                    "INSERT INTO leases (name, holder, expires_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT (name) DO UPDATE SET holder = excluded.holder, expires_at = excluded.expires_at
                     WHERE leases.holder = excluded.holder OR leases.expires_at <= ?4",
                )
                .bind(name)
                .bind(holder)
                .bind(expires_at)
                .bind(now)
                .execute(connection)
                .await?;
                Ok(result.rows_affected() == 1)
            })
        })
        .await
    }

    /// Hands the lease on `name` over at once, if `holder` holds it.
    pub async fn release_lease(&self, name: &str, holder: &str) -> Result<(), WriteError> {
        let (name, holder) = (name.to_owned(), holder.to_owned());
        self.write_waiting(move |connection| {
            Box::pin(async move {
                sqlx::query("UPDATE leases SET expires_at = 0 WHERE name = ? AND holder = ?")
                    .bind(name)
                    .bind(holder)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .await
    }

    /// Creates signed fixtures for database tests. Production finalization
    /// must use `settle_event` so evidence and signature cannot diverge.
    #[cfg(test)]
    async fn record_attestation(
        &self,
        event_id: Uuid,
        attestation: MaybeScalar,
    ) -> Result<bool, WriteError> {
        let attestation = attestation.serialize().to_vec();
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let result = sqlx::query(
                    "UPDATE events SET attestation = ?, updated_at = unixepoch()
                     WHERE id = ? AND attestation IS NULL",
                )
                .bind(attestation)
                .bind(event_id.to_string())
                .execute(connection)
                .await?;
                Ok(result.rows_affected() == 1)
            })
        })
        .await
    }

    /// Finalizes the readings, all entry scores, and attestation together.
    /// Competing or delayed workers cannot change an already signed event.
    pub async fn settle_event(
        &self,
        event_id: Uuid,
        readings: Vec<Reading>,
        scores: Vec<EntryScore>,
        attestation: MaybeScalar,
        clock: crate::oracle::Clock,
    ) -> Result<SettlementOutcome, WriteError> {
        let attestation = attestation.serialize().to_vec();
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
                let event_id = event_id.to_string();
                let Some(event) = sqlx::query(
                    "SELECT signing_date, attestation IS NOT NULL AS attested FROM events WHERE id = ?",
                )
                .bind(&event_id)
                .fetch_optional(&mut *transaction)
                .await? else {
                    transaction.rollback().await?;
                    return Ok(SettlementOutcome::Unchanged);
                };
                if event.try_get::<bool, _>("attested")? {
                    transaction.rollback().await?;
                    return Ok(SettlementOutcome::Unchanged);
                }
                let expires_at = event.try_get::<i64, _>("signing_date")?
                    .saturating_add(EXPIRY_AFTER_SIGNING.whole_seconds());
                if clock().unix_timestamp() >= expires_at {
                    transaction.rollback().await?;
                    return Ok(SettlementOutcome::Expired);
                }
                let result = sqlx::query(
                    "UPDATE events SET attestation = ?, updated_at = unixepoch()
                     WHERE id = ? AND attestation IS NULL",
                )
                .bind(attestation)
                .bind(&event_id)
                .execute(&mut *transaction)
                .await?;
                if result.rows_affected() == 0 {
                    transaction.rollback().await?;
                    return Ok(SettlementOutcome::Unchanged);
                }
                let entry_count: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM events_entries WHERE event_id = ?")
                        .bind(&event_id)
                        .fetch_one(&mut *transaction)
                        .await?;
                let distinct_ids: std::collections::HashSet<_> =
                    scores.iter().map(|score| score.id).collect();
                if scores.is_empty()
                    || entry_count != scores.len() as i64
                    || distinct_ids.len() != scores.len()
                {
                    return Err(sqlx::Error::Protocol(
                        "settlement scores must cover every event entry exactly once".into(),
                    ));
                }
                for score in scores {
                    let result = sqlx::query(
                        "UPDATE events_entries
                         SET score = ?, base_score = ?, updated_at = unixepoch()
                         WHERE event_id = ? AND id = ?",
                    )
                    .bind(score.total_score)
                    .bind(score.base_score)
                    .bind(&event_id)
                    .bind(score.id.to_string())
                    .execute(&mut *transaction)
                    .await?;
                    if result.rows_affected() != 1 {
                        return Err(sqlx::Error::Protocol(
                            "settlement score does not belong to this event".into(),
                        ));
                    }
                }
                sqlx::query("DELETE FROM event_readings WHERE event_id = ?")
                    .bind(&event_id)
                    .execute(&mut *transaction)
                    .await?;
                for reading in readings {
                    sqlx::query(
                        "INSERT INTO event_readings (event_id, target, metric, baseline, observed)
                         VALUES (?, ?, ?, ?, ?)",
                    )
                    .bind(&event_id)
                    .bind(reading.target)
                    .bind(reading.metric)
                    .bind(reading.baseline)
                    .bind(reading.observed)
                    .execute(&mut *transaction)
                    .await?;
                }
                sqlx::query("DELETE FROM event_settlement_blocks WHERE event_id = ?")
                    .bind(&event_id)
                    .execute(&mut *transaction)
                    .await?;
                // Score and reading writes can themselves cross the deadline.
                // Roll back all evidence instead of publishing a late signature.
                if clock().unix_timestamp() >= expires_at {
                    transaction.rollback().await?;
                    return Ok(SettlementOutcome::Expired);
                }
                transaction.commit().await?;
                Ok(SettlementOutcome::Attested)
            })
        })
        .await
    }

    /// Stores provisional scores, without changing signed events.
    pub async fn update_entry_scores(&self, scores: Vec<EntryScore>) -> Result<(), WriteError> {
        if scores.is_empty() {
            return Ok(());
        }
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                for score in scores {
                    sqlx::query(
                        "UPDATE events_entries
                         SET score = ?, base_score = ?, updated_at = unixepoch()
                         WHERE id = ? AND EXISTS (
                             SELECT 1 FROM events
                             WHERE events.id = events_entries.event_id
                               AND events.attestation IS NULL
                         )",
                    )
                    .bind(score.total_score)
                    .bind(score.base_score)
                    .bind(score.id.to_string())
                    .execute(&mut *transaction)
                    .await?;
                }
                transaction.commit().await
            })
        })
        .await
    }

    /// Replaces an event's readings with the latest values, one row per
    /// `(target, metric)`, without changing signed events.
    pub async fn replace_readings(
        &self,
        event_id: Uuid,
        readings: Vec<Reading>,
    ) -> Result<(), WriteError> {
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
                let unsigned: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM events WHERE id = ? AND attestation IS NULL)",
                )
                .bind(event_id.to_string())
                .fetch_one(&mut *transaction)
                .await?;
                if !unsigned {
                    return transaction.rollback().await;
                }
                sqlx::query("DELETE FROM event_readings WHERE event_id = ?")
                    .bind(event_id.to_string())
                    .execute(&mut *transaction)
                    .await?;
                for reading in &readings {
                    sqlx::query(
                        "INSERT INTO event_readings (event_id, target, metric, baseline, observed)
                         VALUES (?, ?, ?, ?, ?)",
                    )
                    .bind(event_id.to_string())
                    .bind(&reading.target)
                    .bind(&reading.metric)
                    .bind(reading.baseline)
                    .bind(reading.observed)
                    .execute(&mut *transaction)
                    .await?;
                }
                transaction.commit().await
            })
        })
        .await
    }

    /// Stored readings for each of `event_ids`.
    pub async fn readings(
        &self,
        event_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<Reading>>, sqlx::Error> {
        if event_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = vec!["?"; event_ids.len()].join(",");
        // Only `?` placeholders are interpolated; ids are bound below.
        let sql = format!(
            "SELECT event_id, target, metric, baseline, observed FROM event_readings
             WHERE event_id IN ({placeholders}) ORDER BY event_id, target, metric"
        );
        let mut query = sqlx::query(AssertSqlSafe(sql));
        for id in event_ids {
            query = query.bind(id.to_string());
        }
        let mut readings: HashMap<Uuid, Vec<Reading>> = HashMap::new();
        for row in query.fetch_all(&self.readers).await? {
            readings
                .entry(uuid_column(&row, "event_id")?)
                .or_default()
                .push(Reading {
                    target: row.try_get("target")?,
                    metric: row.try_get("metric")?,
                    baseline: row.try_get("baseline")?,
                    observed: row.try_get("observed")?,
                });
        }
        Ok(readings)
    }
}

impl DatabaseWriter {
    /// Runs accepted commands until `shutdown` is cancelled, then drains the
    /// queue and closes every connection. Commands accepted before shutdown
    /// still run even when their callers have disconnected.
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<()> {
        let result = loop {
            let command = tokio::select! {
                biased;
                () = shutdown.cancelled() => break Ok(()),
                command = self.commands.recv() => match command {
                    Some(command) => command,
                    None => break Err(anyhow::anyhow!("database writer queue closed unexpectedly")),
                },
            };
            command(&mut self.connection).await;
        };
        self.ready.store(false, Ordering::Release);
        self.commands.close();
        while let Some(command) = self.commands.recv().await {
            command(&mut self.connection).await;
        }
        self.readers.close().await;
        let close = self.connection.close().await;
        result?;
        close.context("close writable SQLite connection")
    }
}

fn command<T, F>(operation: F) -> (Command, oneshot::Receiver<Result<T, sqlx::Error>>)
where
    T: Send + 'static,
    F: for<'a> FnOnce(&'a mut SqliteConnection) -> BoxFuture<'a, Result<T, sqlx::Error>>
        + Send
        + 'static,
{
    let (reply, response) = oneshot::channel();
    let command: Command = Box::new(move |connection| {
        Box::pin(async move {
            let _ = reply.send(operation(connection).await);
        })
    });
    (command, response)
}

/// Result of finalization, including an admitted operation that crossed expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementOutcome {
    Attested,
    Unchanged,
    Expired,
}

/// Events waiting for the oracle's attestation; see
/// [`Database::awaiting_attestation`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AwaitingAttestation {
    pub count: usize,
    /// Unsigned events with entries past their DLC expiry.
    pub expired: usize,
    pub oldest_signing_date: Option<OffsetDateTime>,
    /// Events among `count` whose latest check failed because the published
    /// observations do not cover their window, or because a scored metric
    /// has no baseline.
    pub blocked_on_source_coverage: usize,
    /// Events among `blocked_on_source_coverage` without a baseline: they
    /// can never be attested.
    pub unsettleable: usize,
    /// The earliest signing date among the events in `count` that do not
    /// wait on observations.
    pub oldest_attestable_signing_date: Option<OffsetDateTime>,
}

/// Scores for one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryScore {
    pub id: Uuid,
    pub total_score: i64,
    pub base_score: i64,
}

/// Columns every [`EventRecord`] query selects; callers add `WHERE`,
/// `GROUP BY e.id`, and ordering.
const EVENT_SELECT: &str = "SELECT e.id, e.source, e.signing_date, e.start_observation_date,
        e.end_observation_date, e.locations, e.metrics, e.total_allowed_entries,
        e.number_of_places_win, e.number_of_values_per_entry, e.nonce_salt,
        e.nonce_point, e.coordinator_pubkey, e.attestation, e.unlisted, e.scoring_rules,
        e.settled_without_entries_at, COUNT(ee.id) AS total_entries
     FROM events e
     LEFT JOIN events_entries ee ON ee.event_id = e.id";

/// Events on the public list: not created unlisted.
const LISTED: &str = "e.unlisted = 0";

/// The SQL form of [`EventRecord::status`], and how many times it binds the
/// current time.
fn status_condition(status: EventStatus) -> (&'static str, usize) {
    match status {
        EventStatus::Signed => ("e.attestation IS NOT NULL", 0),
        EventStatus::Live => ("e.attestation IS NULL AND ? < e.start_observation_date", 1),
        EventStatus::Running => (
            "e.attestation IS NULL AND e.start_observation_date <= ? \
             AND ? < e.end_observation_date",
            2,
        ),
        EventStatus::Completed => ("e.attestation IS NULL AND e.end_observation_date <= ?", 1),
    }
}

/// Column values for a new event, encoded before the command is queued so the
/// writer only performs SQL.
struct EventInsert {
    id: String,
    source: String,
    total_allowed_entries: i64,
    number_of_places_win: i64,
    number_of_values_per_entry: i64,
    signing_date: i64,
    start_observation_date: i64,
    end_observation_date: i64,
    nonce_salt: Vec<u8>,
    nonce_point: Vec<u8>,
    event_announcement: Vec<u8>,
    locations: String,
    metrics: String,
    coordinator_pubkey: String,
    unlisted: bool,
    scoring_rules: &'static str,
}

impl EventInsert {
    fn encode(event: &NewEvent) -> Result<Self, WriteError> {
        let count = |value: usize| {
            i64::try_from(value)
                .map_err(|error| WriteError::from(sqlx::Error::Encode(Box::new(error))))
        };
        Ok(Self {
            id: event.id.to_string(),
            source: event.source.clone(),
            total_allowed_entries: count(event.total_allowed_entries)?,
            number_of_places_win: count(event.number_of_places_win)?,
            number_of_values_per_entry: count(event.number_of_values_per_entry)?,
            signing_date: event.signing_date.unix_timestamp(),
            start_observation_date: event.start_observation_date.unix_timestamp(),
            end_observation_date: event.end_observation_date.unix_timestamp(),
            nonce_salt: event.nonce.salt.to_vec(),
            nonce_point: event.nonce.point.serialize().to_vec(),
            event_announcement: serde_json::to_vec(&event.event_announcement)
                .map_err(encode_error)?,
            locations: serde_json::to_string(&event.locations).map_err(encode_error)?,
            metrics: serde_json::to_string(&event.metrics).map_err(encode_error)?,
            coordinator_pubkey: event.coordinator_pubkey.clone(),
            unlisted: event.unlisted,
            scoring_rules: event.scoring_rules.as_str(),
        })
    }
}

fn event_from_row(row: &SqliteRow) -> Result<EventRecord, sqlx::Error> {
    let salt: Vec<u8> = row.try_get("nonce_salt")?;
    let point: Vec<u8> = row.try_get("nonce_point")?;
    let attestation: Option<Vec<u8>> = row.try_get("attestation")?;
    let settled_without_entries_at: Option<i64> = row.try_get("settled_without_entries_at")?;
    Ok(EventRecord {
        id: uuid_column(row, "id")?,
        source: row.try_get("source")?,
        signing_date: timestamp(row.try_get("signing_date")?, "signing_date")?,
        start_observation_date: timestamp(
            row.try_get("start_observation_date")?,
            "start_observation_date",
        )?,
        end_observation_date: timestamp(
            row.try_get("end_observation_date")?,
            "end_observation_date",
        )?,
        locations: json_column(row, "locations")?,
        metrics: json_column(row, "metrics")?,
        number_of_values_per_entry: count_column(row, "number_of_values_per_entry")?,
        total_allowed_entries: count_column(row, "total_allowed_entries")?,
        number_of_places_win: count_column(row, "number_of_places_win")?,
        total_entries: count_column(row, "total_entries")?,
        nonce: EventNonce {
            salt: salt
                .as_slice()
                .try_into()
                .map_err(|error| decode_error("nonce_salt", error))?,
            point: Point::from_slice(&point).map_err(|error| decode_error("nonce_point", error))?,
        },
        coordinator_pubkey: row.try_get("coordinator_pubkey")?,
        attestation: attestation
            .map(|bytes| {
                MaybeScalar::from_slice(&bytes).map_err(|error| decode_error("attestation", error))
            })
            .transpose()?,
        unlisted: row.try_get("unlisted")?,
        settled_without_entries_at: settled_without_entries_at
            .map(|seconds| timestamp(seconds, "settled_without_entries_at"))
            .transpose()?,
        scoring_rules: {
            let rules: String = row.try_get("scoring_rules")?;
            ScoringRules::from_storage(&rules).ok_or_else(|| {
                decode_error("scoring_rules", format!("unknown scoring rules {rules:?}"))
            })?
        },
    })
}

/// A [`Line`] from a row with the `event_lines` or `line_fits` columns.
fn line_from_row(row: &SqliteRow) -> Result<Line, sqlx::Error> {
    let level: String = row.try_get("level")?;
    Ok(Line {
        target: row.try_get("target")?,
        metric: row.try_get("metric")?,
        lower: row.try_get("lower")?,
        upper: row.try_get("upper")?,
        level: LineLevel::from_storage(&level)
            .ok_or_else(|| decode_error("level", format!("unknown line level {level:?}")))?,
        window_hours: row.try_get("window_hours")?,
        windows: row.try_get("windows")?,
        over: row.try_get("over")?,
        par: row.try_get("par")?,
        under: row.try_get("under")?,
        first_window: timestamp(row.try_get("first_window")?, "first_window")?,
        last_window: timestamp(row.try_get("last_window")?, "last_window")?,
        fitted_at: timestamp(row.try_get("fitted_at")?, "fitted_at")?,
    })
}

fn encode_error(error: serde_json::Error) -> WriteError {
    WriteError::from(sqlx::Error::Encode(Box::new(error)))
}

fn decode_error(
    column: &str,
    error: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: column.to_string(),
        source: error.into(),
    }
}

fn timestamp(seconds: i64, column: &str) -> Result<OffsetDateTime, sqlx::Error> {
    OffsetDateTime::from_unix_timestamp(seconds).map_err(|error| decode_error(column, error))
}

fn count_column(row: &SqliteRow, column: &str) -> Result<usize, sqlx::Error> {
    let value: i64 = row.try_get(column)?;
    usize::try_from(value).map_err(|error| decode_error(column, error))
}

fn uuid_column(row: &SqliteRow, column: &str) -> Result<Uuid, sqlx::Error> {
    let value: String = row.try_get(column)?;
    Uuid::parse_str(&value).map_err(|error| decode_error(column, error))
}

fn json_column<T: DeserializeOwned>(row: &SqliteRow, column: &str) -> Result<T, sqlx::Error> {
    let bytes: Vec<u8> = row.try_get(column)?;
    serde_json::from_slice(&bytes).map_err(|error| decode_error(column, error))
}

mod lines;
mod publication;

pub use publication::DuePublication;
#[cfg(test)]
mod tests;

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
