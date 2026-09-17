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
    secp::{MaybeScalar, Scalar},
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

use crate::events::{
    ActiveEvent, CreateEventData, Event, EventFilter, EventSummary, Forecasted, Observed,
    ScoringField, SignEvent, ValueOptions, Weather, WeatherChoices, WeatherEntry, get_status,
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

    pub async fn add_event(&self, event: CreateEventData) -> Result<Event, WriteError> {
        let row = EventInsert::encode(&event)?;
        self.write(move |connection| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO events (
                        id, total_allowed_entries, number_of_places_win,
                        number_of_values_per_entry, nonce, signing_date,
                        start_observation_date, end_observation_date,
                        locations, event_announcement, coordinator_pubkey,
                        scoring_fields
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(row.id)
                .bind(row.total_allowed_entries)
                .bind(row.number_of_places_win)
                .bind(row.number_of_values_per_entry)
                .bind(row.nonce)
                .bind(row.signing_date)
                .bind(row.start_observation_date)
                .bind(row.end_observation_date)
                .bind(row.locations)
                .bind(row.event_announcement)
                .bind(row.coordinator_pubkey)
                .bind(row.scoring_fields)
                .execute(connection)
                .await?;
                Ok(())
            })
        })
        .await?;
        Ok(event.into())
    }

    pub async fn add_event_entries(&self, entries: Vec<WeatherEntry>) -> Result<(), WriteError> {
        self.write(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                for entry in &entries {
                    sqlx::query("INSERT INTO events_entries (id, event_id) VALUES (?, ?)")
                        .bind(entry.id.to_string())
                        .bind(entry.event_id.to_string())
                        .execute(&mut *transaction)
                        .await?;
                    for choice in &entry.expected_observations {
                        sqlx::query(
                            "INSERT INTO expected_observations
                             (entry_id, station, temp_low, temp_high, wind_speed,
                              wind_direction, rain_amt, snow_amt, humidity)
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                        )
                        .bind(entry.id.to_string())
                        .bind(&choice.stations)
                        .bind(choice.temp_low.as_ref().map(ToString::to_string))
                        .bind(choice.temp_high.as_ref().map(ToString::to_string))
                        .bind(choice.wind_speed.as_ref().map(ToString::to_string))
                        .bind(choice.wind_direction.as_ref().map(ToString::to_string))
                        .bind(choice.rain_amt.as_ref().map(ToString::to_string))
                        .bind(choice.snow_amt.as_ref().map(ToString::to_string))
                        .bind(choice.humidity.as_ref().map(ToString::to_string))
                        .execute(&mut *transaction)
                        .await?;
                    }
                }
                transaction.commit().await
            })
        })
        .await
    }

    pub async fn get_event(&self, id: &Uuid) -> Result<Option<Event>, sqlx::Error> {
        let Some(row) = sqlx::query(
            "SELECT id, signing_date, start_observation_date, end_observation_date,
                    event_announcement, locations, total_allowed_entries,
                    number_of_places_win, number_of_values_per_entry,
                    attestation_signature, nonce, coordinator_pubkey, scoring_fields
             FROM events WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.readers)
        .await?
        else {
            return Ok(None);
        };
        let mut event = event_from_row(&row)?;
        event.entries = self.get_event_weather_entries(id).await?;
        event.entry_ids = event.entries.iter().map(|entry| entry.id).collect();
        event.weather = self.get_event_weather(*id).await?;
        Ok(Some(event))
    }

    pub async fn get_event_weather_entries(
        &self,
        event_id: &Uuid,
    ) -> Result<Vec<WeatherEntry>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, score, base_score FROM events_entries WHERE event_id = ? ORDER BY id",
        )
        .bind(event_id.to_string())
        .fetch_all(&self.readers)
        .await?;
        let mut choices = self.get_entry_choices(event_id).await?;
        rows.iter()
            .map(|row| {
                let id = uuid_column(row, "id")?;
                Ok(WeatherEntry {
                    id,
                    event_id: *event_id,
                    score: score_column(row, "score")?,
                    base_score: score_column(row, "base_score")?,
                    expected_observations: choices.remove(&id).unwrap_or_default(),
                })
            })
            .collect()
    }

    async fn get_entry_choices(
        &self,
        event_id: &Uuid,
    ) -> Result<HashMap<Uuid, Vec<WeatherChoices>>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT eo.entry_id, eo.station, eo.temp_low, eo.temp_high, eo.wind_speed,
                    eo.wind_direction, eo.rain_amt, eo.snow_amt, eo.humidity
             FROM expected_observations eo
             JOIN events_entries ee ON ee.id = eo.entry_id
             WHERE ee.event_id = ?
             ORDER BY eo.id",
        )
        .bind(event_id.to_string())
        .fetch_all(&self.readers)
        .await?;
        let mut choices: HashMap<Uuid, Vec<WeatherChoices>> = HashMap::new();
        for row in &rows {
            let entry_id = uuid_column(row, "entry_id")?;
            choices.entry(entry_id).or_default().push(WeatherChoices {
                stations: row.try_get("station")?,
                temp_low: choice_column(row, "temp_low")?,
                temp_high: choice_column(row, "temp_high")?,
                wind_speed: choice_column(row, "wind_speed")?,
                wind_direction: choice_column(row, "wind_direction")?,
                rain_amt: choice_column(row, "rain_amt")?,
                snow_amt: choice_column(row, "snow_amt")?,
                humidity: choice_column(row, "humidity")?,
            });
        }
        Ok(choices)
    }

    pub async fn get_weather_entry(
        &self,
        event_id: &Uuid,
        entry_id: &Uuid,
    ) -> Result<Option<WeatherEntry>, sqlx::Error> {
        Ok(self
            .get_event_weather_entries(event_id)
            .await?
            .into_iter()
            .find(|entry| entry.id == *entry_id))
    }

    /// Events without an attestation, with their entry counts.
    pub async fn get_active_events(&self) -> Result<Vec<ActiveEvent>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT e.id, e.signing_date, e.start_observation_date, e.end_observation_date,
                    e.locations, e.total_allowed_entries, e.number_of_places_win,
                    e.number_of_values_per_entry, e.attestation_signature,
                    e.scoring_fields, COUNT(ee.id) AS total_entries
             FROM events e
             LEFT JOIN events_entries ee ON ee.event_id = e.id
             WHERE e.attestation_signature IS NULL
             GROUP BY e.id
             ORDER BY e.id",
        )
        .fetch_all(&self.readers)
        .await?;
        rows.iter()
            .map(|row| {
                let dates = EventDates::from_row(row)?;
                let attestation: Option<MaybeScalar> = json_column(row, "attestation_signature")?;
                Ok(ActiveEvent {
                    id: uuid_column(row, "id")?,
                    locations: json_column(row, "locations")?,
                    signing_date: dates.signing,
                    start_observation_date: dates.start,
                    end_observation_date: dates.end,
                    status: get_status(attestation, dates.start, dates.end),
                    total_allowed_entries: row.try_get("total_allowed_entries")?,
                    total_entries: row.try_get("total_entries")?,
                    number_of_values_per_entry: row.try_get("number_of_values_per_entry")?,
                    number_of_places_win: row.try_get("number_of_places_win")?,
                    attestation,
                    scoring_fields: scoring_fields_column(row)?,
                })
            })
            .collect()
    }

    pub async fn get_events_to_sign(
        &self,
        event_ids: &[Uuid],
    ) -> Result<Vec<SignEvent>, sqlx::Error> {
        if event_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = vec!["?"; event_ids.len()].join(",");
        let sql = format!(
            "SELECT id, signing_date, start_observation_date, end_observation_date,
                    number_of_places_win, number_of_values_per_entry,
                    attestation_signature, nonce, event_announcement
             FROM events
             WHERE attestation_signature IS NULL AND id IN ({placeholders})
             ORDER BY id"
        );
        // Only `?` placeholders are interpolated; ids are bound below.
        let mut query = sqlx::query(AssertSqlSafe(sql));
        for id in event_ids {
            query = query.bind(id.to_string());
        }
        let rows = query.fetch_all(&self.readers).await?;
        rows.iter()
            .map(|row| {
                let dates = EventDates::from_row(row)?;
                let attestation: Option<MaybeScalar> = json_column(row, "attestation_signature")?;
                Ok(SignEvent {
                    id: uuid_column(row, "id")?,
                    signing_date: dates.signing,
                    start_observation_date: dates.start,
                    end_observation_date: dates.end,
                    status: get_status(attestation, dates.start, dates.end),
                    nonce: json_column(row, "nonce")?,
                    event_announcement: json_column(row, "event_announcement")?,
                    number_of_places_win: row.try_get("number_of_places_win")?,
                    number_of_values_per_entry: row.try_get("number_of_values_per_entry")?,
                    attestation,
                })
            })
            .collect()
    }

    pub async fn update_event_attestation(
        &self,
        event_id: Uuid,
        attestation: MaybeScalar,
    ) -> Result<(), WriteError> {
        let attestation_bytes = serde_json::to_vec(&attestation).map_err(encode_error)?;
        self.write_waiting(move |connection| {
            Box::pin(async move {
                sqlx::query("UPDATE events SET attestation_signature = ? WHERE id = ?")
                    .bind(attestation_bytes)
                    .bind(event_id.to_string())
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .await
    }

    /// Stores `(entry_id, score, base_score)` triples in one transaction.
    pub async fn update_entry_scores(
        &self,
        entry_scores: Vec<(Uuid, i64, i64)>,
    ) -> Result<(), WriteError> {
        if entry_scores.is_empty() {
            return Ok(());
        }
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                for (entry_id, score, base_score) in entry_scores {
                    sqlx::query("UPDATE events_entries SET score = ?, base_score = ? WHERE id = ?")
                        .bind(score)
                        .bind(base_score)
                        .bind(entry_id.to_string())
                        .execute(&mut *transaction)
                        .await?;
                }
                transaction.commit().await
            })
        })
        .await
    }

    pub async fn filtered_list_events(
        &self,
        filter: EventFilter,
    ) -> Result<Vec<EventSummary>, sqlx::Error> {
        let mut events = self.get_filtered_event_summaries(filter).await?;
        for event in &mut events {
            event.weather = self.get_event_weather(event.id).await?;
        }
        Ok(events)
    }

    async fn get_filtered_event_summaries(
        &self,
        filter: EventFilter,
    ) -> Result<Vec<EventSummary>, sqlx::Error> {
        let mut sql = String::from(
            "SELECT e.id, e.signing_date, e.start_observation_date, e.end_observation_date,
                    e.locations, e.total_allowed_entries, e.number_of_places_win,
                    e.number_of_values_per_entry, e.attestation_signature, e.nonce,
                    COUNT(ee.id) AS total_entries
             FROM events e
             LEFT JOIN events_entries ee ON ee.event_id = e.id",
        );
        let event_ids = filter.event_ids.unwrap_or_default();
        if !event_ids.is_empty() {
            let placeholders = vec!["?"; event_ids.len()].join(",");
            sql.push_str(&format!(" WHERE e.id IN ({placeholders})"));
        }
        sql.push_str(" GROUP BY e.id ORDER BY e.id");
        if filter.limit.is_some() {
            sql.push_str(" LIMIT ?");
        }
        // Only `?` placeholders are interpolated; ids and the limit are bound below.
        let mut query = sqlx::query(AssertSqlSafe(sql));
        for id in &event_ids {
            query = query.bind(id.to_string());
        }
        if let Some(limit) = filter.limit {
            query = query.bind(i64::try_from(limit).unwrap_or(i64::MAX));
        }
        let rows = query.fetch_all(&self.readers).await?;
        rows.iter()
            .map(|row| {
                let dates = EventDates::from_row(row)?;
                let attestation: Option<MaybeScalar> = json_column(row, "attestation_signature")?;
                Ok(EventSummary {
                    id: uuid_column(row, "id")?,
                    signing_date: dates.signing,
                    start_observation_date: dates.start,
                    end_observation_date: dates.end,
                    locations: json_column(row, "locations")?,
                    number_of_values_per_entry: row.try_get("number_of_values_per_entry")?,
                    status: get_status(attestation, dates.start, dates.end),
                    total_allowed_entries: row.try_get("total_allowed_entries")?,
                    total_entries: row.try_get("total_entries")?,
                    number_of_places_win: row.try_get("number_of_places_win")?,
                    weather: vec![],
                    attestation,
                    nonce: json_column(row, "nonce")?,
                })
            })
            .collect()
    }

    /// Records the readings taken for an event in one transaction.
    pub async fn update_weather_station_data(
        &self,
        event_id: Uuid,
        weather: Vec<Weather>,
    ) -> Result<(), WriteError> {
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                for reading in &weather {
                    let weather_id = Uuid::now_v7();
                    let observed = reading.observed.as_ref();
                    sqlx::query(
                        "INSERT INTO weather (
                            id, station_id, observed_date, observed_temp_low,
                            observed_temp_high, observed_wind_speed,
                            forecasted_date, forecasted_temp_low,
                            forecasted_temp_high, forecasted_wind_speed
                        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(weather_id.to_string())
                    .bind(&reading.station_id)
                    .bind(observed.map(|value| value.date.unix_timestamp()))
                    .bind(observed.map(|value| value.temp_low))
                    .bind(observed.map(|value| value.temp_high))
                    .bind(observed.map(|value| value.wind_speed))
                    .bind(reading.forecasted.date.unix_timestamp())
                    .bind(reading.forecasted.temp_low)
                    .bind(reading.forecasted.temp_high)
                    .bind(reading.forecasted.wind_speed)
                    .execute(&mut *transaction)
                    .await?;
                    sqlx::query(
                        "INSERT INTO events_weather (id, event_id, weather_id) VALUES (?, ?, ?)",
                    )
                    .bind(Uuid::now_v7().to_string())
                    .bind(event_id.to_string())
                    .bind(weather_id.to_string())
                    .execute(&mut *transaction)
                    .await?;
                }
                transaction.commit().await
            })
        })
        .await
    }

    pub async fn get_event_weather(&self, event_id: Uuid) -> Result<Vec<Weather>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT w.station_id, w.observed_date, w.observed_temp_low, w.observed_temp_high,
                    w.observed_wind_speed, w.forecasted_date, w.forecasted_temp_low,
                    w.forecasted_temp_high, w.forecasted_wind_speed
             FROM weather w
             JOIN events_weather ew ON ew.weather_id = w.id
             WHERE ew.event_id = ?
             ORDER BY w.id",
        )
        .bind(event_id.to_string())
        .fetch_all(&self.readers)
        .await?;
        rows.iter()
            .map(|row| {
                let observed = match row.try_get::<Option<i64>, _>("observed_date")? {
                    Some(date) => Some(Observed {
                        date: timestamp(date, "observed_date")?,
                        temp_low: row.try_get("observed_temp_low")?,
                        temp_high: row.try_get("observed_temp_high")?,
                        wind_speed: row.try_get("observed_wind_speed")?,
                    }),
                    None => None,
                };
                Ok(Weather {
                    station_id: row.try_get("station_id")?,
                    observed,
                    forecasted: Forecasted {
                        date: timestamp(row.try_get("forecasted_date")?, "forecasted_date")?,
                        temp_low: row.try_get("forecasted_temp_low")?,
                        temp_high: row.try_get("forecasted_temp_high")?,
                        wind_speed: row.try_get("forecasted_wind_speed")?,
                    },
                })
            })
            .collect()
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

/// Column values for a new event, encoded before the command is queued so the
/// writer only performs SQL.
struct EventInsert {
    id: String,
    total_allowed_entries: i64,
    number_of_places_win: i64,
    number_of_values_per_entry: i64,
    nonce: Vec<u8>,
    signing_date: i64,
    start_observation_date: i64,
    end_observation_date: i64,
    locations: String,
    event_announcement: Vec<u8>,
    coordinator_pubkey: String,
    scoring_fields: String,
}

impl EventInsert {
    fn encode(event: &CreateEventData) -> Result<Self, WriteError> {
        Ok(Self {
            id: event.id.to_string(),
            total_allowed_entries: event.total_allowed_entries,
            number_of_places_win: event.number_of_places_win,
            number_of_values_per_entry: event.number_of_values_per_entry,
            nonce: serde_json::to_vec(&event.nonce).map_err(encode_error)?,
            signing_date: event.signing_date.unix_timestamp(),
            start_observation_date: event.start_observation_date.unix_timestamp(),
            end_observation_date: event.end_observation_date.unix_timestamp(),
            locations: serde_json::to_string(&event.locations).map_err(encode_error)?,
            event_announcement: serde_json::to_vec(&event.event_announcement)
                .map_err(encode_error)?,
            coordinator_pubkey: event.coordinator_pubkey.clone(),
            scoring_fields: serde_json::to_string(&event.scoring_fields).map_err(encode_error)?,
        })
    }
}

struct EventDates {
    signing: OffsetDateTime,
    start: OffsetDateTime,
    end: OffsetDateTime,
}

impl EventDates {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            signing: timestamp(row.try_get("signing_date")?, "signing_date")?,
            start: timestamp(
                row.try_get("start_observation_date")?,
                "start_observation_date",
            )?,
            end: timestamp(row.try_get("end_observation_date")?, "end_observation_date")?,
        })
    }
}

fn event_from_row(row: &SqliteRow) -> Result<Event, sqlx::Error> {
    let dates = EventDates::from_row(row)?;
    let attestation: Option<MaybeScalar> = json_column(row, "attestation_signature")?;
    let nonce: Scalar = json_column(row, "nonce")?;
    let event_announcement: EventLockingConditions = json_column(row, "event_announcement")?;
    let coordinator_pubkey: Option<String> = row.try_get("coordinator_pubkey")?;
    Ok(Event {
        id: uuid_column(row, "id")?,
        signing_date: dates.signing,
        start_observation_date: dates.start,
        end_observation_date: dates.end,
        locations: json_column(row, "locations")?,
        number_of_values_per_entry: row.try_get("number_of_values_per_entry")?,
        status: get_status(attestation, dates.start, dates.end),
        total_allowed_entries: row.try_get("total_allowed_entries")?,
        entry_ids: vec![],
        number_of_places_win: row.try_get("number_of_places_win")?,
        entries: vec![],
        weather: vec![],
        nonce,
        event_announcement,
        attestation,
        coordinator_pubkey: coordinator_pubkey.unwrap_or_default(),
        scoring_fields: scoring_fields_column(row)?,
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

fn uuid_column(row: &SqliteRow, column: &str) -> Result<Uuid, sqlx::Error> {
    let value: String = row.try_get(column)?;
    Uuid::parse_str(&value).map_err(|error| decode_error(column, error))
}

/// Decodes a JSON column stored as TEXT or BLOB. A NULL column decodes as
/// `None` for optional targets and as an error for required ones.
fn json_column<T: DeserializeOwned>(row: &SqliteRow, column: &str) -> Result<T, sqlx::Error> {
    let bytes: Option<Vec<u8>> = row.try_get(column)?;
    let bytes = bytes.unwrap_or_else(|| b"null".to_vec());
    serde_json::from_slice(&bytes).map_err(|error| decode_error(column, error))
}

fn scoring_fields_column(row: &SqliteRow) -> Result<Vec<ScoringField>, sqlx::Error> {
    let fields: Option<Vec<ScoringField>> = json_column(row, "scoring_fields")?;
    Ok(fields
        .filter(|fields| !fields.is_empty())
        .unwrap_or_else(ScoringField::defaults))
}

fn choice_column(row: &SqliteRow, column: &str) -> Result<Option<ValueOptions>, sqlx::Error> {
    let value: Option<String> = row.try_get(column)?;
    value
        .map(|value| {
            ValueOptions::try_from(value.as_str()).map_err(|error| decode_error(column, error))
        })
        .transpose()
}

/// Scores are stored as `0` until the ETL computes them; callers see `None`.
fn score_column(row: &SqliteRow, column: &str) -> Result<Option<i64>, sqlx::Error> {
    let value: Option<i64> = row.try_get(column)?;
    Ok(value.filter(|score| *score != 0))
}

#[cfg(test)]
mod tests;
