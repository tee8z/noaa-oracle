use anyhow::{Context, Result};
use dlctix::secp::{MaybeScalar, Scalar};
use dlctix::{musig2::secp256k1::XOnlyPublicKey, EventLockingConditions};
use log::info;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions},
    Row,
};
use std::{future::Future, path::Path, str::FromStr, time::Duration};
use time::OffsetDateTime;
use tokio::{
    fs::create_dir_all,
    sync::{mpsc, oneshot},
};
use uuid::Uuid;

use super::{
    ActiveEvent, CreateEventData, Event, EventFilter, EventSummary, Forecasted, Observed,
    ScoringField, SignEvent, ValueOptions, Weather, WeatherChoices, WeatherEntry,
};

type WriteOperation = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

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

pub struct Database {
    pool: SqlitePool,
    writer: DatabaseWriter,
}

impl Clone for Database {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            writer: DatabaseWriter::new(),
        }
    }
}

impl Database {
    pub async fn new(path: &str) -> Result<Self> {
        let db_path = format!("{}/events.sqlite", path);

        if let Some(parent) = Path::new(&db_path).parent() {
            create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create database directory: {parent:?}"))?;
        }

        let options = SqliteConnectOptions::from_str(&format!("sqlite:{}", db_path))?
            .create_if_missing(true)
            .pragma("journal_mode", "WAL")
            .pragma("synchronous", "NORMAL")
            .pragma("busy_timeout", "5000")
            .pragma("cache_size", "-64000")
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
            writer: DatabaseWriter::new(),
        };

        db.run_migrations().await?;
        info!("SQLite database initialized at: {}", db_path);

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

    /// Check database connectivity and integrity.
    pub async fn health_check(&self) -> Result<()> {
        // Basic connectivity
        sqlx::query("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .context("Database connectivity check failed")?;

        // Page structure integrity
        let result: String = sqlx::query_scalar("PRAGMA quick_check;")
            .fetch_one(&self.pool)
            .await
            .context("Database integrity check failed")?;
        if result != "ok" {
            return Err(anyhow::anyhow!(
                "Database integrity check failed: {}",
                result
            ));
        }

        Ok(())
    }

    /// Checkpoint WAL to main database file before shutdown.
    /// This ensures all pending writes are flushed so Litestream
    /// can replicate a complete database to S3.
    pub async fn checkpoint(&self) {
        match sqlx::query("PRAGMA wal_checkpoint(TRUNCATE);")
            .execute(&self.pool)
            .await
        {
            Ok(_) => info!("WAL checkpoint completed successfully"),
            Err(e) => log::error!("WAL checkpoint failed: {}", e),
        }
    }

    pub async fn add_oracle_metadata(&self, pubkey: XOnlyPublicKey) -> Result<()> {
        let pool = self.pool.clone();
        let pubkey_bytes = pubkey.serialize().to_vec();
        let name = "4casttruth".to_string();

        self.writer
            .execute(pool, move |pool| async move {
                sqlx::query(
                    "INSERT INTO oracle_metadata (pubkey, name) VALUES (?, ?)
                     ON CONFLICT(pubkey) DO NOTHING",
                )
                .bind(&pubkey_bytes)
                .bind(&name)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
    }

    pub async fn get_stored_public_key(&self) -> Result<XOnlyPublicKey> {
        let row: (Vec<u8>,) = sqlx::query_as("SELECT pubkey FROM oracle_metadata LIMIT 1")
            .fetch_one(&self.pool)
            .await?;

        XOnlyPublicKey::from_slice(&row.0).map_err(|e| anyhow::anyhow!("Invalid pubkey: {}", e))
    }

    pub async fn add_event(&self, event: CreateEventData) -> Result<Event> {
        let pool = self.pool.clone();
        let event_clone = event.clone();

        self.writer
            .execute(pool, move |pool| async move {
                let locations_json = serde_json::to_string(&event.locations)?;
                let nonce_bytes = serde_json::to_vec(&event.nonce)?;
                let announcement_bytes = serde_json::to_vec(&event.event_announcement)?;
                let scoring_fields_json = serde_json::to_string(&event.scoring_fields)?;

                sqlx::query(
                    "INSERT INTO events (
                        id, total_allowed_entries, number_of_places_win,
                        number_of_values_per_entry, nonce, signing_date,
                        start_observation_date, end_observation_date,
                        locations, event_announcement, coordinator_pubkey,
                        scoring_fields
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(event.id.to_string())
                .bind(event.total_allowed_entries)
                .bind(event.number_of_places_win)
                .bind(event.number_of_values_per_entry)
                .bind(&nonce_bytes)
                .bind(event.signing_date.unix_timestamp())
                .bind(event.start_observation_date.unix_timestamp())
                .bind(event.end_observation_date.unix_timestamp())
                .bind(&locations_json)
                .bind(&announcement_bytes)
                .bind(&event.coordinator_pubkey)
                .bind(&scoring_fields_json)
                .execute(&pool)
                .await?;

                Ok(event_clone.into())
            })
            .await
    }

    pub async fn add_event_entries(&self, entries: Vec<WeatherEntry>) -> Result<()> {
        let pool = self.pool.clone();

        self.writer
            .execute(pool, move |pool| async move {
                let mut tx = pool.begin().await?;

                for entry in entries {
                    sqlx::query("INSERT INTO events_entries (id, event_id) VALUES (?, ?)")
                        .bind(entry.id.to_string())
                        .bind(entry.event_id.to_string())
                        .execute(&mut *tx)
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
                        .bind(choice.temp_low.as_ref().map(|v| v.to_string()))
                        .bind(choice.temp_high.as_ref().map(|v| v.to_string()))
                        .bind(choice.wind_speed.as_ref().map(|v| v.to_string()))
                        .bind(choice.wind_direction.as_ref().map(|v| v.to_string()))
                        .bind(choice.rain_amt.as_ref().map(|v| v.to_string()))
                        .bind(choice.snow_amt.as_ref().map(|v| v.to_string()))
                        .bind(choice.humidity.as_ref().map(|v| v.to_string()))
                        .execute(&mut *tx)
                        .await?;
                    }
                }

                tx.commit().await?;
                Ok(())
            })
            .await
    }

    pub async fn add_entry(&self, entry: WeatherEntry) -> Result<()> {
        self.add_event_entries(vec![entry]).await
    }

    pub async fn get_event(&self, id: &Uuid) -> Result<Event> {
        let mut event = self.get_basic_event(id).await?;
        event.entries = self.get_event_weather_entries(id).await?;
        event.entry_ids = event.entries.iter().map(|e| e.id).collect();
        event.weather = self.get_event_weather(*id).await?;
        Ok(event)
    }

    async fn get_basic_event(&self, id: &Uuid) -> Result<Event> {
        let row = sqlx::query(
            "SELECT id, signing_date, start_observation_date, end_observation_date,
                    event_announcement, locations, total_allowed_entries,
                    number_of_places_win, number_of_values_per_entry,
                    attestation_signature, nonce, coordinator_pubkey, scoring_fields
             FROM events WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_one(&self.pool)
        .await?;

        self.row_to_event(&row)
    }

    fn row_to_event(&self, row: &sqlx::sqlite::SqliteRow) -> Result<Event> {
        let id: String = row.get("id");
        let signing_ts: i64 = row.get("signing_date");
        let start_ts: i64 = row.get("start_observation_date");
        let end_ts: i64 = row.get("end_observation_date");
        let announcement_bytes: Vec<u8> = row.get("event_announcement");
        let locations_json: String = row.get("locations");
        let nonce_bytes: Vec<u8> = row.get("nonce");
        let attestation_bytes: Option<Vec<u8>> = row.get("attestation_signature");
        let coordinator_pubkey: Option<String> = row.get("coordinator_pubkey");
        let scoring_fields_json: Option<String> = row.get("scoring_fields");

        let signing_date = OffsetDateTime::from_unix_timestamp(signing_ts)?;
        let start_observation_date = OffsetDateTime::from_unix_timestamp(start_ts)?;
        let end_observation_date = OffsetDateTime::from_unix_timestamp(end_ts)?;

        let locations: Vec<String> = serde_json::from_str(&locations_json)?;
        let nonce: Scalar = serde_json::from_slice(&nonce_bytes)?;
        let event_announcement: EventLockingConditions =
            serde_json::from_slice(&announcement_bytes)?;
        let attestation: Option<MaybeScalar> = attestation_bytes
            .as_ref()
            .and_then(|b| serde_json::from_slice(b).ok());
        let scoring_fields: Vec<ScoringField> = scoring_fields_json
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_else(ScoringField::defaults);

        let status = super::get_status(attestation, start_observation_date, end_observation_date);

        Ok(Event {
            id: Uuid::parse_str(&id)?,
            signing_date,
            start_observation_date,
            end_observation_date,
            locations,
            number_of_values_per_entry: row.get("number_of_values_per_entry"),
            status,
            total_allowed_entries: row.get("total_allowed_entries"),
            entry_ids: vec![],
            number_of_places_win: row.get("number_of_places_win"),
            entries: vec![],
            weather: vec![],
            nonce,
            event_announcement,
            attestation,
            coordinator_pubkey: coordinator_pubkey.unwrap_or_default(),
            scoring_fields,
        })
    }

    pub async fn get_event_weather_entries(&self, event_id: &Uuid) -> Result<Vec<WeatherEntry>> {
        let rows = sqlx::query(
            "SELECT id, event_id, score, base_score
             FROM events_entries WHERE event_id = ?",
        )
        .bind(event_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut entries = Vec::new();
        for row in rows {
            let entry_id: String = row.get("id");
            let entry_uuid = Uuid::parse_str(&entry_id)?;

            let choices = self.get_entry_choices(&entry_uuid).await?;

            entries.push(WeatherEntry {
                id: entry_uuid,
                event_id: *event_id,
                score: row.get::<Option<i64>, _>("score").filter(|&s| s != 0),
                base_score: row.get::<Option<i64>, _>("base_score").filter(|&s| s != 0),
                expected_observations: choices,
            });
        }

        Ok(entries)
    }

    async fn get_entry_choices(&self, entry_id: &Uuid) -> Result<Vec<WeatherChoices>> {
        let rows = sqlx::query(
            "SELECT station, temp_low, temp_high, wind_speed,
                    wind_direction, rain_amt, snow_amt, humidity
             FROM expected_observations WHERE entry_id = ?",
        )
        .bind(entry_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut choices = Vec::new();
        for row in rows {
            choices.push(WeatherChoices {
                stations: row.get("station"),
                temp_low: row
                    .get::<Option<String>, _>("temp_low")
                    .and_then(|s| ValueOptions::try_from(s).ok()),
                temp_high: row
                    .get::<Option<String>, _>("temp_high")
                    .and_then(|s| ValueOptions::try_from(s).ok()),
                wind_speed: row
                    .get::<Option<String>, _>("wind_speed")
                    .and_then(|s| ValueOptions::try_from(s).ok()),
                wind_direction: row
                    .get::<Option<String>, _>("wind_direction")
                    .and_then(|s| ValueOptions::try_from(s).ok()),
                rain_amt: row
                    .get::<Option<String>, _>("rain_amt")
                    .and_then(|s| ValueOptions::try_from(s).ok()),
                snow_amt: row
                    .get::<Option<String>, _>("snow_amt")
                    .and_then(|s| ValueOptions::try_from(s).ok()),
                humidity: row
                    .get::<Option<String>, _>("humidity")
                    .and_then(|s| ValueOptions::try_from(s).ok()),
            });
        }

        Ok(choices)
    }

    pub async fn get_active_events(&self) -> Result<Vec<ActiveEvent>> {
        let rows = sqlx::query(
            "SELECT e.id, e.signing_date, e.start_observation_date, e.end_observation_date,
                    e.locations, e.total_allowed_entries, e.number_of_places_win,
                    e.number_of_values_per_entry, e.attestation_signature,
                    e.scoring_fields, COUNT(ee.id) as total_entries
             FROM events e
             LEFT JOIN events_entries ee ON ee.event_id = e.id
             WHERE e.attestation_signature IS NULL
             GROUP BY e.id",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::new();
        for row in rows {
            let id: String = row.get("id");
            let signing_ts: i64 = row.get("signing_date");
            let start_ts: i64 = row.get("start_observation_date");
            let end_ts: i64 = row.get("end_observation_date");
            let locations_json: String = row.get("locations");
            let attestation_bytes: Option<Vec<u8>> = row.get("attestation_signature");
            let scoring_fields_json: Option<String> = row.get("scoring_fields");

            let signing_date = OffsetDateTime::from_unix_timestamp(signing_ts)?;
            let start_observation_date = OffsetDateTime::from_unix_timestamp(start_ts)?;
            let end_observation_date = OffsetDateTime::from_unix_timestamp(end_ts)?;
            let locations: Vec<String> = serde_json::from_str(&locations_json)?;
            let attestation: Option<MaybeScalar> = attestation_bytes
                .as_ref()
                .and_then(|b| serde_json::from_slice(b).ok());
            let scoring_fields: Vec<ScoringField> = scoring_fields_json
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_else(ScoringField::defaults);

            let status =
                super::get_status(attestation, start_observation_date, end_observation_date);

            events.push(ActiveEvent {
                id: Uuid::parse_str(&id)?,
                locations,
                signing_date,
                start_observation_date,
                end_observation_date,
                status,
                total_allowed_entries: row.get("total_allowed_entries"),
                total_entries: row.get("total_entries"),
                number_of_values_per_entry: row.get("number_of_values_per_entry"),
                number_of_places_win: row.get("number_of_places_win"),
                attestation,
                scoring_fields,
            });
        }

        Ok(events)
    }

    pub async fn get_events_to_sign(&self, event_ids: Vec<Uuid>) -> Result<Vec<SignEvent>> {
        if event_ids.is_empty() {
            return Ok(vec![]);
        }

        let placeholders: String = event_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT id, signing_date, start_observation_date, end_observation_date,
                    number_of_places_win, number_of_values_per_entry,
                    attestation_signature, nonce, event_announcement
             FROM events
             WHERE attestation_signature IS NULL AND id IN ({})",
            placeholders
        );

        let mut q = sqlx::query(&query);
        for id in &event_ids {
            q = q.bind(id.to_string());
        }

        let rows = q.fetch_all(&self.pool).await?;
        let mut events = Vec::new();

        for row in rows {
            let id: String = row.get("id");
            let signing_ts: i64 = row.get("signing_date");
            let start_ts: i64 = row.get("start_observation_date");
            let end_ts: i64 = row.get("end_observation_date");
            let nonce_bytes: Vec<u8> = row.get("nonce");
            let announcement_bytes: Vec<u8> = row.get("event_announcement");
            let attestation_bytes: Option<Vec<u8>> = row.get("attestation_signature");

            let signing_date = OffsetDateTime::from_unix_timestamp(signing_ts)?;
            let start_observation_date = OffsetDateTime::from_unix_timestamp(start_ts)?;
            let end_observation_date = OffsetDateTime::from_unix_timestamp(end_ts)?;

            let nonce: Scalar = serde_json::from_slice(&nonce_bytes)?;
            let event_announcement: EventLockingConditions =
                serde_json::from_slice(&announcement_bytes)?;
            let attestation: Option<MaybeScalar> = attestation_bytes
                .as_ref()
                .and_then(|b| serde_json::from_slice(b).ok());

            let status =
                super::get_status(attestation, start_observation_date, end_observation_date);

            events.push(SignEvent {
                id: Uuid::parse_str(&id)?,
                signing_date,
                start_observation_date,
                end_observation_date,
                status,
                nonce,
                event_announcement,
                number_of_places_win: row.get("number_of_places_win"),
                number_of_values_per_entry: row.get("number_of_values_per_entry"),
                attestation,
            });
        }

        Ok(events)
    }

    pub async fn update_event_attestation(&self, event: &SignEvent) -> Result<()> {
        let Some(attestation) = event.attestation else {
            return Err(anyhow::anyhow!("No attestation to update"));
        };

        let pool = self.pool.clone();
        let event_id = event.id.to_string();
        let attestation_bytes = serde_json::to_vec(&attestation)?;

        self.writer
            .execute(pool, move |pool| async move {
                sqlx::query("UPDATE events SET attestation_signature = ? WHERE id = ?")
                    .bind(&attestation_bytes)
                    .bind(&event_id)
                    .execute(&pool)
                    .await?;
                Ok(())
            })
            .await
    }

    pub async fn update_entry_scores(&self, entry_scores: Vec<(Uuid, i64, i64)>) -> Result<()> {
        if entry_scores.is_empty() {
            return Ok(());
        }

        let pool = self.pool.clone();

        self.writer
            .execute(pool, move |pool| async move {
                let mut tx = pool.begin().await?;

                for (entry_id, score, base_score) in entry_scores {
                    sqlx::query("UPDATE events_entries SET score = ?, base_score = ? WHERE id = ?")
                        .bind(score)
                        .bind(base_score)
                        .bind(entry_id.to_string())
                        .execute(&mut *tx)
                        .await?;
                }

                tx.commit().await?;
                Ok(())
            })
            .await
    }

    pub async fn get_event_coordinator_pubkey(&self, event_id: Uuid) -> Result<String> {
        let row: (Option<String>,) =
            sqlx::query_as("SELECT coordinator_pubkey FROM events WHERE id = ?")
                .bind(event_id.to_string())
                .fetch_one(&self.pool)
                .await?;

        row.0
            .ok_or_else(|| anyhow::anyhow!("Coordinator pubkey not found"))
    }

    pub async fn filtered_list_events(&self, filter: EventFilter) -> Result<Vec<EventSummary>> {
        let mut events = self.get_filtered_event_summaries(filter).await?;
        for event in events.iter_mut() {
            event.weather = self.get_event_weather(event.id).await?;
        }
        Ok(events)
    }

    async fn get_filtered_event_summaries(&self, filter: EventFilter) -> Result<Vec<EventSummary>> {
        let mut query = String::from(
            "SELECT e.id, e.signing_date, e.start_observation_date, e.end_observation_date,
                    e.locations, e.total_allowed_entries, e.number_of_places_win,
                    e.number_of_values_per_entry, e.attestation_signature, e.nonce,
                    COUNT(ee.id) as total_entries
             FROM events e
             LEFT JOIN events_entries ee ON ee.event_id = e.id",
        );

        let mut conditions = Vec::new();
        let mut bindings: Vec<String> = Vec::new();

        if let Some(ref ids) = filter.event_ids {
            let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            conditions.push(format!("e.id IN ({})", placeholders));
            bindings.extend(ids.iter().map(|id| id.to_string()));
        }

        if !conditions.is_empty() {
            query.push_str(" WHERE ");
            query.push_str(&conditions.join(" AND "));
        }

        query.push_str(" GROUP BY e.id");

        if let Some(limit) = filter.limit {
            query.push_str(&format!(" LIMIT {}", limit));
        }

        let mut q = sqlx::query(&query);
        for binding in &bindings {
            q = q.bind(binding);
        }

        let rows = q.fetch_all(&self.pool).await?;
        let mut events = Vec::new();

        for row in rows {
            let id: String = row.get("id");
            let signing_ts: i64 = row.get("signing_date");
            let start_ts: i64 = row.get("start_observation_date");
            let end_ts: i64 = row.get("end_observation_date");
            let locations_json: String = row.get("locations");
            let nonce_bytes: Vec<u8> = row.get("nonce");
            let attestation_bytes: Option<Vec<u8>> = row.get("attestation_signature");

            let signing_date = OffsetDateTime::from_unix_timestamp(signing_ts)?;
            let start_observation_date = OffsetDateTime::from_unix_timestamp(start_ts)?;
            let end_observation_date = OffsetDateTime::from_unix_timestamp(end_ts)?;

            let locations: Vec<String> = serde_json::from_str(&locations_json)?;
            let nonce: Scalar = serde_json::from_slice(&nonce_bytes)?;
            let attestation: Option<MaybeScalar> = attestation_bytes
                .as_ref()
                .and_then(|b| serde_json::from_slice(b).ok());

            let status =
                super::get_status(attestation, start_observation_date, end_observation_date);

            events.push(EventSummary {
                id: Uuid::parse_str(&id)?,
                signing_date,
                start_observation_date,
                end_observation_date,
                locations,
                number_of_values_per_entry: row.get("number_of_values_per_entry"),
                status,
                total_allowed_entries: row.get("total_allowed_entries"),
                total_entries: row.get("total_entries"),
                number_of_places_win: row.get("number_of_places_win"),
                weather: vec![],
                attestation,
                nonce,
            });
        }

        Ok(events)
    }

    pub async fn add_weather_readings(&self, weather: Vec<Weather>) -> Result<Vec<Uuid>> {
        let pool = self.pool.clone();

        self.writer
            .execute(pool, move |pool| async move {
                let mut tx = pool.begin().await?;
                let mut weather_ids = Vec::new();

                for w in weather {
                    let weather_id = Uuid::now_v7();
                    weather_ids.push(weather_id);

                    let (obs_date, obs_low, obs_high, obs_wind) = match &w.observed {
                        Some(obs) => (
                            Some(obs.date.unix_timestamp()),
                            Some(obs.temp_low),
                            Some(obs.temp_high),
                            Some(obs.wind_speed),
                        ),
                        None => (None, None, None, None),
                    };

                    sqlx::query(
                        "INSERT INTO weather (
                            id, station_id, observed_date, observed_temp_low,
                            observed_temp_high, observed_wind_speed,
                            forecasted_date, forecasted_temp_low,
                            forecasted_temp_high, forecasted_wind_speed
                        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(weather_id.to_string())
                    .bind(&w.station_id)
                    .bind(obs_date)
                    .bind(obs_low)
                    .bind(obs_high)
                    .bind(obs_wind)
                    .bind(w.forecasted.date.unix_timestamp())
                    .bind(w.forecasted.temp_low)
                    .bind(w.forecasted.temp_high)
                    .bind(w.forecasted.wind_speed)
                    .execute(&mut *tx)
                    .await?;
                }

                tx.commit().await?;
                Ok(weather_ids)
            })
            .await
    }

    pub async fn batch_add_weather_to_event(
        &self,
        event_id: Uuid,
        weather_ids: Vec<Uuid>,
    ) -> Result<()> {
        let pool = self.pool.clone();

        self.writer
            .execute(pool, move |pool| async move {
                let mut tx = pool.begin().await?;

                for weather_id in weather_ids {
                    let junction_id = Uuid::now_v7();
                    sqlx::query(
                        "INSERT INTO events_weather (id, event_id, weather_id) VALUES (?, ?, ?)",
                    )
                    .bind(junction_id.to_string())
                    .bind(event_id.to_string())
                    .bind(weather_id.to_string())
                    .execute(&mut *tx)
                    .await?;
                }

                tx.commit().await?;
                Ok(())
            })
            .await
    }

    pub async fn update_weather_station_data(
        &self,
        event_id: Uuid,
        weather: Vec<Weather>,
    ) -> Result<()> {
        let weather_ids = self.add_weather_readings(weather).await?;
        self.batch_add_weather_to_event(event_id, weather_ids).await
    }

    pub async fn get_event_weather(&self, event_id: Uuid) -> Result<Vec<Weather>> {
        let rows = sqlx::query(
            "SELECT w.station_id, w.observed_date, w.observed_temp_low, w.observed_temp_high,
                    w.observed_wind_speed, w.forecasted_date, w.forecasted_temp_low,
                    w.forecasted_temp_high, w.forecasted_wind_speed
             FROM weather w
             JOIN events_weather ew ON ew.weather_id = w.id
             WHERE ew.event_id = ?",
        )
        .bind(event_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut weather = Vec::new();
        for row in rows {
            let observed = match row.get::<Option<i64>, _>("observed_date") {
                Some(date) => Some(Observed {
                    date: OffsetDateTime::from_unix_timestamp(date)?,
                    temp_low: row.get("observed_temp_low"),
                    temp_high: row.get("observed_temp_high"),
                    wind_speed: row.get("observed_wind_speed"),
                }),
                None => None,
            };

            let forecasted_date: i64 = row.get("forecasted_date");
            let forecasted = Forecasted {
                date: OffsetDateTime::from_unix_timestamp(forecasted_date)?,
                temp_low: row.get("forecasted_temp_low"),
                temp_high: row.get("forecasted_temp_high"),
                wind_speed: row.get("forecasted_wind_speed"),
            };

            weather.push(Weather {
                station_id: row.get("station_id"),
                observed,
                forecasted,
            });
        }

        Ok(weather)
    }

    pub async fn get_weather_entry(
        &self,
        event_id: &Uuid,
        entry_id: &Uuid,
    ) -> Result<WeatherEntry> {
        let row = sqlx::query(
            "SELECT id, event_id, score, base_score
             FROM events_entries
             WHERE id = ? AND event_id = ?",
        )
        .bind(entry_id.to_string())
        .bind(event_id.to_string())
        .fetch_one(&self.pool)
        .await?;

        let choices = self.get_entry_choices(entry_id).await?;

        Ok(WeatherEntry {
            id: *entry_id,
            event_id: *event_id,
            score: row.get::<Option<i64>, _>("score").filter(|&s| s != 0),
            base_score: row.get::<Option<i64>, _>("base_score").filter(|&s| s != 0),
            expected_observations: choices,
        })
    }
}
