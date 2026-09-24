//! Oracle behaviour: event creation, entry intake, scoring, and
//! attestation. Persistence goes through [`Database`]; readings come from
//! the event's [`OutcomeSource`]; the key never leaves [`SigningKey`].

use base64::{Engine, engine::general_purpose};
use dlctix::secp::Point;
use log::{error, info, warn};
use nostr::{key::PublicKey as NostrPublicKey, nips::nip19::ToBech32};
use std::{path::Path, sync::Arc};
use time::OffsetDateTime;
use tokio::task::JoinError;
use uuid::Uuid;

use crate::{
    database::{Database, EntryScore, WriteError},
    events::{
        AddEventEntry, CreateEvent, EntryRejection, Event, EventCounts, EventFilter,
        EventListQuery, EventRecord, EventRejection, EventStatus, EventSummary, NewEvent,
        WeatherEntry, validate_entries,
    },
    scoring::{self, NotUuidV7, Scored},
    signing::{AttestError, KeyError, SigningKey},
    sources::{ObservationWindow, OutcomeSource, SourceError, Sources},
};

/// The current time. Injected so tests can move through an event's
/// lifecycle without sleeping.
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(OffsetDateTime::now_utc)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("event {0} not found")]
    EventNotFound(Uuid),
    #[error("entry {entry} not found for event {event}")]
    EntryNotFound { event: Uuid, entry: Uuid },
    #[error(transparent)]
    InvalidEvent(#[from] EventRejection),
    #[error(transparent)]
    InvalidEntries(#[from] EntryRejection),
    #[error("invalid event filter: {0}")]
    InvalidFilter(#[from] uuid::Error),
    #[error("event {0} belongs to another coordinator")]
    WrongCoordinator(Uuid),
    #[error("event {event} uses source {source_id:?}, which this oracle does not run")]
    UnknownSource { event: Uuid, source_id: String },
    #[error("failed to load the oracle key")]
    Key(#[from] KeyError),
    #[error("database was created with key {stored}, but the configured key is {configured}")]
    KeyMismatch { stored: String, configured: String },
    #[error("failed to read events")]
    Read(#[source] Box<sqlx::Error>),
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error("failed to read source data")]
    Source(#[from] SourceError),
    #[error("refused to attest event {event}")]
    Attest { event: Uuid, source: AttestError },
    #[error(transparent)]
    Score(#[from] NotUuidV7),
    #[error("background computation failed")]
    Task(#[from] JoinError),
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Error::Read(Box::new(error))
    }
}

pub struct Oracle {
    db: Database,
    sources: Sources,
    key: Arc<SigningKey>,
    clock: Clock,
}

impl Oracle {
    /// Loads (or creates) the signing key and checks it matches the key the
    /// database was created with.
    pub async fn new(
        db: Database,
        sources: Sources,
        private_key_file_path: &Path,
        clock: Clock,
    ) -> Result<Self, Error> {
        let path = private_key_file_path.to_owned();
        let key = tokio::task::spawn_blocking(move || SigningKey::load_or_create(&path)).await??;
        let oracle = Self {
            db,
            sources,
            key: Arc::new(key),
            clock,
        };
        oracle.check_stored_key().await?;
        Ok(oracle)
    }

    async fn check_stored_key(&self) -> Result<(), Error> {
        let own_key = self.key.x_only_public_key();
        match self.db.get_stored_public_key().await? {
            Some(stored) if stored != own_key => Err(Error::KeyMismatch {
                stored: stored.to_string(),
                configured: own_key.to_string(),
            }),
            Some(_) => Ok(()),
            None => Ok(self.db.add_oracle_metadata(own_key).await?),
        }
    }

    pub fn public_key(&self) -> Point {
        Point::from(self.key.public_key())
    }

    /// Compressed public key, base64 encoded.
    pub fn public_key_base64(&self) -> String {
        general_purpose::STANDARD.encode(self.public_key().serialize())
    }

    pub fn npub(&self) -> String {
        self.key.npub()
    }

    pub fn sources(&self) -> &Sources {
        &self.sources
    }

    fn now(&self) -> OffsetDateTime {
        (self.clock)()
    }

    fn source_for(&self, event: &EventRecord) -> Result<&Arc<dyn OutcomeSource>, Error> {
        self.sources
            .get(&event.source)
            .ok_or_else(|| Error::UnknownSource {
                event: event.id,
                source_id: event.source.clone(),
            })
    }

    pub async fn list_events(&self, filter: EventFilter) -> Result<Vec<EventSummary>, Error> {
        let records = self
            .db
            .list_events(&filter.event_ids()?, filter.limit())
            .await?;
        let ids: Vec<Uuid> = records.iter().map(|record| record.id).collect();
        let mut readings = self.db.readings(&ids).await?;
        let now = self.now();
        Ok(records
            .into_iter()
            .map(|record| {
                let readings = readings.remove(&record.id).unwrap_or_default();
                record.into_summary(now, &readings)
            })
            .collect())
    }

    /// One page of the UI's events list. Rows carry no weather readings;
    /// the list does not show them.
    pub async fn event_page(&self, query: &EventListQuery) -> Result<Vec<EventSummary>, Error> {
        let now = self.now();
        Ok(self
            .db
            .event_page(query, now)
            .await?
            .into_iter()
            .map(|record| record.into_summary(now, &[]))
            .collect())
    }

    /// Events by status, counted like [`Self::event_page`] lists them.
    pub async fn event_counts(&self, include_tests: bool) -> Result<EventCounts, Error> {
        Ok(self.db.event_counts(include_tests, self.now()).await?)
    }

    pub async fn get_event(&self, id: Uuid) -> Result<Event, Error> {
        let record = self
            .db
            .get_event(id)
            .await?
            .ok_or(Error::EventNotFound(id))?;
        let announcement = self
            .db
            .event_announcement(id)
            .await?
            .ok_or(Error::EventNotFound(id))?;
        let entries = self.db.event_entries(id).await?;
        let readings = self
            .db
            .readings(&[id])
            .await?
            .remove(&id)
            .unwrap_or_default();
        Ok(record.into_event(self.now(), announcement, entries, &readings))
    }

    pub async fn create_event(
        &self,
        coordinator: NostrPublicKey,
        event: CreateEvent,
    ) -> Result<Event, Error> {
        let sources = self.sources.clone();
        let key = self.key.clone();
        // Building the announcement is bounded (MAX_OUTCOMES) but CPU heavy.
        let new_event = tokio::task::spawn_blocking(move || {
            NewEvent::build(event, &sources, &key, coordinator)
        })
        .await??;
        self.db.add_event(&new_event).await?;
        info!(
            "created event {} for {} with {} outcomes",
            new_event.id,
            new_event.coordinator_pubkey,
            new_event.event_announcement.locking_points.len()
        );
        self.get_event(new_event.id).await
    }

    pub async fn add_event_entries(
        &self,
        coordinator: NostrPublicKey,
        event_id: Uuid,
        entries: Vec<AddEventEntry>,
    ) -> Result<Vec<WeatherEntry>, Error> {
        let event = self
            .db
            .get_event(event_id)
            .await?
            .ok_or(Error::EventNotFound(event_id))?;
        let Ok(coordinator) = coordinator.to_bech32();
        if event.coordinator_pubkey != coordinator {
            return Err(Error::WrongCoordinator(event_id));
        }
        let entries = validate_entries(&event, entries, self.now())?;
        if !self.db.add_event_entries(event_id, entries.clone()).await? {
            return Err(EntryRejection::AlreadySubmitted.into());
        }
        Ok(entries.into_iter().map(WeatherEntry::from).collect())
    }

    pub async fn get_event_entry(
        &self,
        event_id: Uuid,
        entry_id: Uuid,
    ) -> Result<WeatherEntry, Error> {
        self.db
            .event_entry(event_id, entry_id)
            .await?
            .map(WeatherEntry::from)
            .ok_or(Error::EntryNotFound {
                event: event_id,
                entry: entry_id,
            })
    }

    /// Refreshes readings, scores entries, and attests events whose signing
    /// date has passed. Safe to repeat: attestation happens at most once per
    /// event. A failing event is logged and does not stop the others.
    /// Returns the number of events that failed.
    pub async fn etl_data(&self, etl_process_id: u64) -> Result<usize, Error> {
        let events = self.db.unattested_events().await?;
        info!("etl {etl_process_id}: {} unattested events", events.len());
        let mut failures = 0;
        for event in events {
            let id = event.id;
            if let Err(error) = self.process_event(event).await {
                failures += 1;
                error!("etl {etl_process_id}: event {id} failed: {error:#}");
            }
        }
        info!("etl {etl_process_id}: done, {failures} failed");
        Ok(failures)
    }

    async fn process_event(&self, event: EventRecord) -> Result<(), Error> {
        let source = self.source_for(&event)?;
        let window = ObservationWindow {
            start: event.start_observation_date,
            end: event.end_observation_date,
        };
        let readings = source.readings(window, &event.locations).await?;
        self.db.replace_readings(event.id, readings.clone()).await?;

        let now = self.now();
        if event.status(now) == EventStatus::Live {
            return Ok(());
        }
        let entries = self.db.event_entries(event.id).await?;
        let scored = entries
            .iter()
            .map(|entry| {
                let base_score =
                    scoring::base_score(&entry.picks, &readings, &event.metrics, |id| {
                        source.metric(id)
                    });
                Ok(Scored {
                    id: entry.id,
                    base_score,
                    total_score: scoring::total_score(entry.id, base_score)?,
                })
            })
            .collect::<Result<Vec<Scored>, NotUuidV7>>()?;
        self.db
            .update_entry_scores(
                scored
                    .iter()
                    .map(|scored| EntryScore {
                        id: scored.id,
                        total_score: scored.total_score,
                        base_score: i64::try_from(scored.base_score).unwrap_or(i64::MAX),
                    })
                    .collect(),
            )
            .await?;

        if event.status(now) == EventStatus::Completed && now >= event.signing_date {
            self.attest(&event, &scored).await?;
        }
        Ok(())
    }

    /// Signs the outcome for `scored`. The attestation is computed from the
    /// same scores that were just stored, and only for an announced outcome.
    async fn attest(&self, event: &EventRecord, scored: &[Scored]) -> Result<(), Error> {
        if scored.is_empty() {
            warn!(
                "event {} reached its signing date without entries",
                event.id
            );
            return Ok(());
        }
        let winners = scoring::winning_indices(scored, event.number_of_places_win);
        let message = scoring::outcome_message(&winners);
        let announcement = self
            .db
            .event_announcement(event.id)
            .await?
            .ok_or(Error::EventNotFound(event.id))?;
        let attestation = self
            .key
            .attest(
                event.id,
                &event.nonce,
                &announcement.locking_points,
                &message,
            )
            .map_err(|source| Error::Attest {
                event: event.id,
                source,
            })?;
        if self.db.record_attestation(event.id, attestation).await? {
            info!("attested event {} with winners {winners:?}", event.id);
        } else {
            warn!(
                "event {} was already attested; kept the stored attestation",
                event.id
            );
        }
        Ok(())
    }
}
