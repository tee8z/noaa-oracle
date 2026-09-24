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
        Entry, EventCounts, EventListQuery, EventRecord, EventStatus, NewEvent, TEST_WINDOW,
        ValueOptions,
    },
    scoring::Pick,
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

    pub async fn add_event(&self, event: &NewEvent) -> Result<(), WriteError> {
        let row = EventInsert::encode(event)?;
        self.write(move |connection| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO events (
                        id, source, total_allowed_entries, number_of_places_win,
                        number_of_values_per_entry, signing_date,
                        start_observation_date, end_observation_date,
                        nonce_salt, nonce_point, event_announcement,
                        locations, metrics, coordinator_pubkey
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(row.id)
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
                .execute(connection)
                .await?;
                Ok(())
            })
        })
        .await
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

    /// A page of the UI's events list, newest first. Test events and the
    /// status are filtered here, before the limit.
    pub async fn event_page(
        &self,
        query: &EventListQuery,
        now: OffsetDateTime,
    ) -> Result<Vec<EventRecord>, sqlx::Error> {
        let now = now.unix_timestamp();
        let mut conditions = vec![];
        let mut binds = vec![];
        if !query.include_tests {
            conditions.push(NOT_A_TEST);
            binds.push(TEST_WINDOW.whole_seconds());
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

    /// Events by status, with or without test events, and the number of test
    /// events either way.
    pub async fn event_counts(
        &self,
        include_tests: bool,
        now: OffsetDateTime,
    ) -> Result<EventCounts, sqlx::Error> {
        let now = now.unix_timestamp();
        let row = sqlx::query(
            "WITH e AS (
                 SELECT attestation, start_observation_date AS start,
                        end_observation_date AS end_, (? OR NOT test) AS counted, test
                 FROM (SELECT *, end_observation_date - start_observation_date < ? AS test
                       FROM events))
             SELECT
                 COALESCE(SUM(counted AND attestation IS NULL AND ? < start), 0) AS live,
                 COALESCE(SUM(counted AND attestation IS NULL AND start <= ? AND ? < end_), 0)
                     AS running,
                 COALESCE(SUM(counted AND attestation IS NULL AND end_ <= ?), 0) AS completed,
                 COALESCE(SUM(counted AND attestation IS NOT NULL), 0) AS signed,
                 COALESCE(SUM(test), 0) AS tests
             FROM e",
        )
        .bind(include_tests)
        .bind(TEST_WINDOW.whole_seconds())
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
            tests: count_column(&row, "tests")?,
        })
    }

    /// Events without an attestation, oldest first.
    pub async fn unattested_events(&self) -> Result<Vec<EventRecord>, sqlx::Error> {
        // Only constant SQL is interpolated.
        let sql = format!("{EVENT_SELECT} WHERE e.attestation IS NULL GROUP BY e.id ORDER BY e.id");
        let rows = sqlx::query(AssertSqlSafe(sql))
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

    /// Stores the attestation unless the event already has one. Returns
    /// whether it was stored. Never overwriting is what guarantees one
    /// signature per nonce.
    pub async fn record_attestation(
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

    /// Stores scores in one transaction.
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
                         WHERE id = ?",
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
    /// `(target, metric)`.
    pub async fn replace_readings(
        &self,
        event_id: Uuid,
        readings: Vec<Reading>,
    ) -> Result<(), WriteError> {
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
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
        e.nonce_point, e.coordinator_pubkey, e.attestation,
        COUNT(ee.id) AS total_entries
     FROM events e
     LEFT JOIN events_entries ee ON ee.event_id = e.id";

/// Events whose window is at least [`TEST_WINDOW`] long (bind its seconds).
const NOT_A_TEST: &str = "e.end_observation_date - e.start_observation_date >= ?";

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
        })
    }
}

fn event_from_row(row: &SqliteRow) -> Result<EventRecord, sqlx::Error> {
    let salt: Vec<u8> = row.try_get("nonce_salt")?;
    let point: Vec<u8> = row.try_get("nonce_point")?;
    let attestation: Option<Vec<u8>> = row.try_get("attestation")?;
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

#[cfg(test)]
mod tests;

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
