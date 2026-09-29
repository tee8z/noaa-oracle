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
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    database::{AwaitingAttestation, Database, EntryScore, WriteError},
    events::{
        AddEventEntry, CreateEvent, EntryRejection, Event, EventCounts, EventFilter,
        EventListQuery, EventRecord, EventRejection, EventStatus, EventSummary, NewEvent,
        SettlementBlock, WeatherEntry, validate_entries,
    },
    lines::{self, Line, LinePass, LineSettings},
    scoring::{self, NotUuidV7, PickRule, Scored, ScoringRules},
    signing::{AttestError, KeyError, SigningKey},
    sources::{ObservationWindow, OutcomeSource, Reading, SourceError, Sources},
    statement::Statement,
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
    #[error("failed to read source data: {0}")]
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

/// What one processing pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EtlSummary {
    /// Events whose processing failed; they are retried next pass.
    pub failed: usize,
    /// Events attested in this pass.
    pub attested: usize,
}

pub struct Oracle {
    db: Database,
    sources: Sources,
    key: Arc<SigningKey>,
    clock: Clock,
    lines: LineSettings,
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
            lines: LineSettings::default(),
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

    /// One line pass for every source (see [`lines::run_pass`]). A failing
    /// source is logged and does not stop the others.
    pub async fn run_line_pass(&self) -> LinePass {
        let now = self.now();
        let mut total = LinePass::default();
        for source in self.sources.all() {
            match lines::run_pass(&self.db, source.as_ref(), &self.lines, now).await {
                Ok(pass) => {
                    total.windows += pass.windows;
                    total.pairs += pass.pairs;
                    total.lines += pass.lines;
                }
                Err(error) => warn!("line pass for {} failed: {error:#}", source.id()),
            }
        }
        total
    }

    /// The lines an event on `source` over `window` would copy if it were
    /// created now, or the target and metric pairs that have none.
    pub async fn current_lines(
        &self,
        source: &str,
        window: ObservationWindow,
        targets: &[String],
        metrics: &[String],
    ) -> Result<Result<Vec<Line>, Vec<String>>, Error> {
        Ok(lines::current(&self.db, &self.lines, source, window, targets, metrics).await?)
    }

    /// The lines `earlier` froze that `new_event`, created with
    /// `lines_from_event`, copies instead of the current fit (see
    /// [`NewEvent::lines_from`]).
    async fn lines_from_event(
        &self,
        new_event: &NewEvent,
        earlier: Uuid,
        window: ObservationWindow,
    ) -> Result<Vec<Line>, Error> {
        let record = self
            .db
            .get_event(earlier)
            .await?
            .ok_or(EventRejection::LinesSourceEventNotFound(earlier))?;
        let frozen = self
            .db
            .event_lines(&[earlier])
            .await?
            .remove(&earlier)
            .unwrap_or_default();
        let window_hours = self.lines.window_hours_for(window.end - window.start);
        Ok(new_event.lines_from(&record, &frozen, window_hours)?)
    }

    /// The oracle's current time.
    pub fn now(&self) -> OffsetDateTime {
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
        let mut blocks = self.db.settlement_blocks(&ids).await?;
        let now = self.now();
        Ok(records
            .into_iter()
            .map(|record| {
                let readings = readings.remove(&record.id).unwrap_or_default();
                let block = blocks.remove(&record.id);
                let mut summary = record.into_summary(now, &readings);
                summary.settlement_block = block;
                summary
            })
            .collect())
    }

    /// One page of the UI's events list. Rows carry no weather readings;
    /// the list does not show them.
    pub async fn event_page(&self, query: &EventListQuery) -> Result<Vec<EventSummary>, Error> {
        let now = self.now();
        let records = self.db.event_page(query, now).await?;
        let ids = records.iter().map(|record| record.id).collect::<Vec<_>>();
        let mut blocks = self.db.settlement_blocks(&ids).await?;
        Ok(records
            .into_iter()
            .map(|record| {
                let block = blocks.remove(&record.id);
                let mut summary = record.into_summary(now, &[]);
                summary.settlement_block = block;
                summary
            })
            .collect())
    }

    /// Events by status, counted like [`Self::event_page`] lists them.
    pub async fn event_counts(&self, include_unlisted: bool) -> Result<EventCounts, Error> {
        Ok(self.db.event_counts(include_unlisted, self.now()).await?)
    }

    /// Events past their observation window that still wait for an
    /// attestation.
    pub async fn awaiting_attestation(&self) -> Result<AwaitingAttestation, Error> {
        Ok(self.db.awaiting_attestation(self.now()).await?)
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
        let block = self.db.settlement_blocks(&[id]).await?.remove(&id);
        let lines = self
            .db
            .event_lines(&[id])
            .await?
            .remove(&id)
            .unwrap_or_default();
        let mut event = record.into_event(self.now(), announcement, entries, &readings, lines);
        event.settlement_block = block;
        event.statement =
            Statement::for_event(&event).map(|statement| self.key.sign_statement(statement));
        Ok(event)
    }

    pub async fn create_event(
        &self,
        coordinator: NostrPublicKey,
        event: CreateEvent,
    ) -> Result<Event, Error> {
        let sources = self.sources.clone();
        let key = self.key.clone();
        let lines_from_event = event.lines_from_event;
        // Building the announcement is bounded (MAX_OUTCOMES) but CPU heavy.
        let mut new_event = tokio::task::spawn_blocking(move || {
            NewEvent::build(event, &sources, &key, coordinator)
        })
        .await??;
        if new_event.scoring_rules == ScoringRules::Lines {
            let window = ObservationWindow {
                start: new_event.start_observation_date,
                end: new_event.end_observation_date,
            };
            new_event.lines = match lines_from_event {
                Some(earlier) => self.lines_from_event(&new_event, earlier, window).await?,
                None => self
                    .current_lines(
                        &new_event.source,
                        window,
                        &new_event.locations,
                        &new_event.metrics,
                    )
                    .await?
                    .map_err(|missing| EventRejection::LinesUnavailable(missing.join(", ")))?,
            };
        }
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
    pub async fn etl_data(&self, etl_process_id: u64) -> Result<EtlSummary, Error> {
        self.etl_data_until(etl_process_id, &CancellationToken::new())
            .await
    }

    /// As [`Self::etl_data`], but stops between events once `stop` is cancelled. A process
    /// shutting down then releases its processing lease without waiting out a long pass, which
    /// its stop timeout could cut short, leaving the next process locked out until the lease
    /// expires. The events left over are read on the next pass.
    pub async fn etl_data_until(
        &self,
        etl_process_id: u64,
        stop: &CancellationToken,
    ) -> Result<EtlSummary, Error> {
        let events = self.db.events_to_settle(self.now()).await?;
        info!("etl {etl_process_id}: {} events to settle", events.len());
        let mut summary = EtlSummary::default();
        let total = events.len();
        for (done, event) in events.into_iter().enumerate() {
            if stop.is_cancelled() {
                info!(
                    "etl {etl_process_id}: stopping; {} events left for the next pass",
                    total - done
                );
                break;
            }
            let id = event.id;
            match self.process_event(event).await {
                Ok(true) => summary.attested += 1,
                Ok(false) => {}
                Err(error) => {
                    summary.failed += 1;
                    error!("etl {etl_process_id}: event {id} failed: {error:#}");
                }
            }
        }
        info!(
            "etl {etl_process_id}: done, {} attested, {} failed",
            summary.attested, summary.failed
        );
        Ok(summary)
    }

    /// Returns whether the event was attested.
    async fn process_event(&self, event: EventRecord) -> Result<bool, Error> {
        let result = self.refresh_event(&event).await;
        if let Err(error) = &result {
            let code = match error {
                Error::Source(SourceError::DataQuality { .. }) => "data_quality",
                Error::Source(SourceError::SettlementBlocked(_)) => "incomplete_readings",
                Error::Source(_) => "source_unavailable",
                _ => "processing_failed",
            };
            self.db
                .set_settlement_block(
                    event.id,
                    SettlementBlock {
                        code: code.into(),
                        message: error.to_string(),
                        checked_at: self.now(),
                    },
                )
                .await?;
        }
        result
    }

    async fn refresh_event(&self, event: &EventRecord) -> Result<bool, Error> {
        let source = self.source_for(event)?;
        let window = ObservationWindow {
            start: event.start_observation_date,
            end: event.end_observation_date,
        };
        let now = self.now();
        let signing_due = event.status(now) == EventStatus::Completed && now >= event.signing_date;
        let readings = if signing_due {
            let readings = source
                .settlement_readings(window, &event.locations, event.signing_date, &event.metrics)
                .await?;
            validate_settlement_readings(event, &readings)?;
            readings
        } else {
            source.readings(window, &event.locations).await?
        };
        // Neither stale saved readings nor progress scores can pass the strict
        // gate. Validation finishes before replacing either stored value.
        if event.status(now) == EventStatus::Live {
            self.db.replace_readings(event.id, readings).await?;
            return Ok(false);
        }
        let entries = self.db.event_entries(event.id).await?;
        let lines = self.scoring_lines(event).await?;
        if signing_due {
            validate_lines(event, &lines)?;
        }
        let rule = |target: &str, metric: &str| match event.scoring_rules {
            ScoringRules::Fixed => source
                .metric(metric)
                .map(|metric| PickRule::Fixed(metric.par)),
            ScoringRules::Lines => lines
                .iter()
                .find(|line| line.target == target && line.metric == metric)
                .map(|line| PickRule::Line(line.band())),
        };
        let scored = entries
            .iter()
            .map(|entry| {
                let base_score = scoring::base_score(&entry.picks, &readings, &event.metrics, rule);
                Ok(Scored {
                    id: entry.id,
                    base_score,
                    total_score: scoring::total_score(entry.id, base_score)?,
                })
            })
            .collect::<Result<Vec<Scored>, NotUuidV7>>()?;
        if signing_due && !scored.is_empty() {
            return self.attest(event, &scored, readings).await;
        }
        self.db.replace_readings(event.id, readings).await?;
        self.db.update_entry_scores(entry_scores(&scored)).await?;
        // No-entry events have no outcome to sign. A successful strict
        // refresh can still clear their block; provisional refreshes cannot.
        if signing_due {
            self.db.clear_settlement_block(event.id).await?;
            warn!(
                "event {} reached its signing date without entries",
                event.id
            );
        }
        Ok(false)
    }

    /// The lines a `lines` event scores against; empty for `fixed` events.
    async fn scoring_lines(&self, event: &EventRecord) -> Result<Vec<Line>, Error> {
        Ok(match event.scoring_rules {
            ScoringRules::Fixed => vec![],
            ScoringRules::Lines => self
                .db
                .event_lines(&[event.id])
                .await?
                .remove(&event.id)
                .unwrap_or_default(),
        })
    }

    /// Signs the outcome for `scored`. The attestation is computed from the
    /// same readings and scores committed with it, and only for an announced outcome.
    /// Returns whether this call stored the attestation.
    async fn attest(
        &self,
        event: &EventRecord,
        scored: &[Scored],
        readings: Vec<Reading>,
    ) -> Result<bool, Error> {
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
        let stored = self
            .db
            .settle_event(event.id, readings, entry_scores(scored), attestation)
            .await?;
        if stored {
            info!("attested event {} with winners {winners:?}", event.id);
        } else {
            warn!(
                "event {} was already attested; kept the stored attestation",
                event.id
            );
        }
        Ok(stored)
    }
}

fn entry_scores(scored: &[Scored]) -> Vec<EntryScore> {
    scored
        .iter()
        .map(|scored| EntryScore {
            id: scored.id,
            total_score: scored.total_score,
            base_score: i64::try_from(scored.base_score).unwrap_or(i64::MAX),
        })
        .collect()
}

/// A `lines` event needs its line for every enabled pair; they are stored
/// with the event, so a missing one means the record is damaged.
fn validate_lines(event: &EventRecord, lines: &[Line]) -> Result<(), SourceError> {
    if event.scoring_rules != ScoringRules::Lines {
        return Ok(());
    }
    let missing: Vec<String> = event
        .locations
        .iter()
        .flat_map(|target| event.metrics.iter().map(move |metric| (target, metric)))
        .filter(|(target, metric)| {
            lines
                .iter()
                .filter(|line| &&line.target == target && &&line.metric == metric)
                .count()
                != 1
        })
        .map(|(target, metric)| format!("{target}/{metric}"))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SourceError::SettlementBlocked(format!(
            "the event's line is missing for: {}",
            missing.join(", ")
        )))
    }
}

/// Every enabled pair must occur exactly once with finite values. An absent
/// station, forecast, metric, or duplicate cannot become an all-zero refund.
fn validate_settlement_readings(
    event: &EventRecord,
    readings: &[Reading],
) -> Result<(), SourceError> {
    let mut missing = Vec::new();
    for target in &event.locations {
        for metric in &event.metrics {
            let matches = readings
                .iter()
                .filter(|reading| &reading.target == target && &reading.metric == metric)
                .collect::<Vec<_>>();
            if matches.len() != 1
                || !matches[0].baseline.is_some_and(f64::is_finite)
                || !matches[0].observed.is_some_and(f64::is_finite)
            {
                missing.push(format!("{target}/{metric}"));
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SourceError::SettlementBlocked(format!(
            "verified baseline and observation required for every enabled pair; missing or ambiguous: {}",
            missing.join(", ")
        )))
    }
}
