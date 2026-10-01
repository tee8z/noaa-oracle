//! Oracle behaviour: event creation, entry intake, scoring, and
//! attestation. Persistence goes through [`Database`]; readings come from
//! the event's [`OutcomeSource`]; the key never leaves [`SigningKey`].

use base64::{Engine, engine::general_purpose};
use dlctix::secp::Point;
use log::{error, info, warn};
use nostr::{key::PublicKey as NostrPublicKey, nips::nip19::ToBech32};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};
use time::{Duration, OffsetDateTime};
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

/// How long an unsigned event's provisional readings stay current. New
/// observations arrive hourly, so reading more often only costs CPU.
const PROVISIONAL_REFRESH: Duration = Duration::minutes(15);

/// How long an event waits after its first failed read. Each further
/// failure doubles the wait, up to [`LONGEST_RETRY`].
const FIRST_RETRY: Duration = Duration::minutes(15);

/// The longest wait between reads of an event that keeps failing. Well
/// inside the day an event has between its signing date and its expiry.
const LONGEST_RETRY: Duration = Duration::hours(6);

/// What one processing pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EtlSummary {
    /// Events whose processing failed; they are retried after a wait that
    /// grows with every failure.
    pub failed: usize,
    /// Events attested in this pass.
    pub attested: usize,
    /// Events read from their source in this pass, whatever came of it.
    pub refreshed: usize,
    /// Events left unread because their observation window has not started.
    pub skipped_live: usize,
    /// Events left unread because they wait out an earlier failure.
    pub skipped_backed_off: usize,
    /// Events left unread because their provisional readings are recent.
    pub skipped_fresh: usize,
    /// Events closed in this pass for having no entries at their signing date.
    pub settled_without_entries: usize,
}

/// A failed read of an event, and how many in a row it makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Failure {
    count: u32,
    at: OffsetDateTime,
}

impl Failure {
    /// When the event may be read again. `None` for a failure from before
    /// the signing date once that date has passed: the first settlement
    /// check is never delayed.
    fn next_attempt(&self, event: &EventRecord, now: OffsetDateTime) -> Option<OffsetDateTime> {
        let before_signing = self.at < event.signing_date && now >= event.signing_date;
        (!before_signing).then(|| self.at + retry_delay(self.count))
    }
}

/// The wait after `failures` failed reads in a row: 15 minutes, doubling
/// to 30 and 60, and so on up to [`LONGEST_RETRY`].
fn retry_delay(failures: u32) -> Duration {
    // Bounded so the multiplication cannot overflow.
    let doublings = failures.saturating_sub(1).min(16);
    (FIRST_RETRY * 2_i32.pow(doublings)).min(LONGEST_RETRY)
}

/// What a processing pass remembers about unsigned events between passes.
/// None of it is stored: a new process reads each event once more than it
/// had to, and takes a failing event's wait from its settlement block.
#[derive(Default)]
struct EtlMemory {
    /// When each event's readings were last refreshed.
    refreshed: HashMap<Uuid, OffsetDateTime>,
    /// Events whose latest read failed.
    failures: HashMap<Uuid, Failure>,
}

/// What a pass does with one unsigned event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The observation window has not started: there is nothing to score.
    SkipLive,
    /// The signing date passed without entries: there is no outcome to sign.
    SettleWithoutEntries,
    /// An earlier read failed and its wait is not over.
    SkipBackedOff,
    /// The provisional readings are recent.
    SkipFresh,
    Read,
}

/// Decides from the event's status before anything is read from its
/// source. Settlement is never held back for fresh provisional readings.
fn next_step(
    event: &EventRecord,
    now: OffsetDateTime,
    refreshed: Option<OffsetDateTime>,
    failure: Option<Failure>,
) -> Step {
    if event.status(now) == EventStatus::Live {
        return Step::SkipLive;
    }
    let signing_due = signing_due(event, now);
    if signing_due && event.total_entries == 0 {
        return Step::SettleWithoutEntries;
    }
    let next_attempt = failure.and_then(|failure| failure.next_attempt(event, now));
    if next_attempt.is_some_and(|next_attempt| now < next_attempt) {
        return Step::SkipBackedOff;
    }
    if !signing_due && refreshed.is_some_and(|refreshed| now < refreshed + PROVISIONAL_REFRESH) {
        return Step::SkipFresh;
    }
    Step::Read
}

fn signing_due(event: &EventRecord, now: OffsetDateTime) -> bool {
    event.status(now) == EventStatus::Completed && now >= event.signing_date
}

pub struct Oracle {
    db: Database,
    sources: Sources,
    key: Arc<SigningKey>,
    clock: Clock,
    lines: LineSettings,
    etl: Mutex<EtlMemory>,
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
            etl: Mutex::default(),
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
    /// event. A failing event is logged and does not stop the others. An
    /// event is read only when its status calls for it: not before its
    /// observation window, and not while it waits out a failure.
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
        let ids: Vec<Uuid> = events.iter().map(|event| event.id).collect();
        let blocks = self.db.settlement_blocks(&ids).await?;
        self.forget_all_but(&ids);
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
            let result = match self.step_for(&event, blocks.get(&id)) {
                Step::SkipLive => {
                    summary.skipped_live += 1;
                    continue;
                }
                Step::SkipBackedOff => {
                    summary.skipped_backed_off += 1;
                    continue;
                }
                Step::SkipFresh => {
                    summary.skipped_fresh += 1;
                    continue;
                }
                Step::SettleWithoutEntries => {
                    let settled = self.settle_without_entries(&event).await;
                    if matches!(settled, Ok(true)) {
                        summary.settled_without_entries += 1;
                    }
                    settled.map(|_| false)
                }
                Step::Read => {
                    summary.refreshed += 1;
                    self.process_event(event).await
                }
            };
            match result {
                Ok(true) => summary.attested += 1,
                Ok(false) => {}
                Err(error) => {
                    summary.failed += 1;
                    error!("etl {etl_process_id}: event {id} failed: {error:#}");
                }
            }
        }
        info!(
            "etl {etl_process_id}: done, {} attested, {} failed, {} refreshed, \
             {} skipped as live, {} skipped as backed off, {} skipped as recently refreshed, \
             {} settled without entries",
            summary.attested,
            summary.failed,
            summary.refreshed,
            summary.skipped_live,
            summary.skipped_backed_off,
            summary.skipped_fresh,
            summary.settled_without_entries
        );
        Ok(summary)
    }

    fn etl_memory(&self) -> MutexGuard<'_, EtlMemory> {
        self.etl.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drops what is remembered about events that left processing: signed,
    /// expired, or settled without entries.
    fn forget_all_but(&self, ids: &[Uuid]) {
        let ids: HashSet<&Uuid> = ids.iter().collect();
        let mut memory = self.etl_memory();
        memory.refreshed.retain(|id, _| ids.contains(id));
        memory.failures.retain(|id, _| ids.contains(id));
    }

    /// The step for `event` now. A new process remembers no failures, so a
    /// stored `block` counts as the event's first.
    fn step_for(&self, event: &EventRecord, block: Option<&SettlementBlock>) -> Step {
        let now = self.now();
        let memory = self.etl_memory();
        let failure = memory.failures.get(&event.id).copied().or_else(|| {
            block.map(|block| Failure {
                count: 1,
                at: block.checked_at,
            })
        });
        next_step(
            event,
            now,
            memory.refreshed.get(&event.id).copied(),
            failure,
        )
    }

    /// Counts a failed read at `now` and returns it. Failures from before
    /// the signing date do not lengthen the wait between settlement checks.
    fn remember_failure(&self, event: &EventRecord, now: OffsetDateTime) -> Failure {
        let mut memory = self.etl_memory();
        let earlier = memory
            .failures
            .get(&event.id)
            .filter(|failure| failure.next_attempt(event, now).is_some())
            .map_or(0, |failure| failure.count);
        let failure = Failure {
            count: earlier.saturating_add(1),
            at: now,
        };
        memory.failures.insert(event.id, failure);
        failure
    }

    fn remember_refresh(&self, event: Uuid, now: OffsetDateTime) {
        let mut memory = self.etl_memory();
        memory.failures.remove(&event);
        memory.refreshed.insert(event, now);
    }

    /// Returns whether the event was attested.
    async fn process_event(&self, event: EventRecord) -> Result<bool, Error> {
        let result = self.refresh_event(&event).await;
        let now = self.now();
        match &result {
            Ok(_) => self.remember_refresh(event.id, now),
            Err(error) => {
                let failure = self.remember_failure(&event, now);
                warn!(
                    "event {}: failure {} in a row; next attempt at {}",
                    event.id,
                    failure.count,
                    now + retry_delay(failure.count)
                );
                let code = match error {
                    Error::Source(SourceError::DataQuality { .. }) => "data_quality",
                    Error::Source(SourceError::SettlementBlocked(_)) => "incomplete_readings",
                    Error::Source(_) => "source_unavailable",
                    _ => "processing_failed",
                };
                let source_coverage = matches!(error, Error::Source(SourceError::Coverage(_)));
                self.db
                    .set_settlement_block(
                        event.id,
                        SettlementBlock {
                            code: code.into(),
                            message: error.to_string(),
                            checked_at: now,
                        },
                        source_coverage,
                    )
                    .await?;
            }
        }
        result
    }

    /// Closes an event that reached its signing date without entries. It
    /// has no outcome to sign, so nothing about its source can hold this
    /// back: the last readings are stored for display when they can be read.
    /// Returns whether this call closed the event.
    async fn settle_without_entries(&self, event: &EventRecord) -> Result<bool, Error> {
        let window = ObservationWindow {
            start: event.start_observation_date,
            end: event.end_observation_date,
        };
        match self.source_for(event) {
            Ok(source) => match source.readings(window, &event.locations).await {
                Ok(readings) => self.db.replace_readings(event.id, readings).await?,
                Err(error) => info!("event {} keeps its stored readings: {error:#}", event.id),
            },
            Err(error) => info!("event {} keeps its stored readings: {error:#}", event.id),
        }
        let settled = self.db.settle_without_entries(event.id, self.now()).await?;
        if settled {
            warn!(
                "event {} reached its signing date without entries; settled without an outcome",
                event.id
            );
        }
        Ok(settled)
    }

    async fn refresh_event(&self, event: &EventRecord) -> Result<bool, Error> {
        let source = self.source_for(event)?;
        let window = ObservationWindow {
            start: event.start_observation_date,
            end: event.end_observation_date,
        };
        let now = self.now();
        let signing_due = signing_due(event, now);
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

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const START: OffsetDateTime = datetime!(2030-01-01 00:00 UTC);

    /// An unsigned event observing a day from [`START`], signed two hours
    /// after its window.
    fn event(entries: usize) -> EventRecord {
        let directory = tempfile::tempdir().unwrap();
        let key = SigningKey::load_or_create(&directory.path().join("oracle.pem")).unwrap();
        let id = Uuid::now_v7();
        EventRecord {
            id,
            source: "noaa_weather".into(),
            signing_date: START + Duration::hours(26),
            start_observation_date: START,
            end_observation_date: START + Duration::hours(24),
            locations: vec!["KORD".into()],
            metrics: vec!["temp_high".into()],
            number_of_values_per_entry: 1,
            total_allowed_entries: 3,
            number_of_places_win: 1,
            nonce: key.new_event_nonce(id),
            coordinator_pubkey: "npub1coordinator".into(),
            attestation: None,
            total_entries: entries,
            unlisted: false,
            scoring_rules: ScoringRules::Fixed,
            settled_without_entries_at: None,
        }
    }

    fn failure(count: u32, at: OffsetDateTime) -> Option<Failure> {
        Some(Failure { count, at })
    }

    #[test]
    fn the_wait_after_a_failure_doubles_up_to_six_hours() {
        let minutes: Vec<i64> = (1..=8)
            .map(|failures| retry_delay(failures).whole_minutes())
            .collect();
        assert_eq!(minutes, vec![15, 30, 60, 120, 240, 360, 360, 360]);
        assert_eq!(retry_delay(0), FIRST_RETRY);
        assert_eq!(retry_delay(u32::MAX), LONGEST_RETRY);
    }

    #[test]
    fn a_live_event_is_skipped_whatever_is_remembered_about_it() {
        let event = event(3);
        let before = START - Duration::SECOND;
        assert_eq!(next_step(&event, before, None, None), Step::SkipLive);
        let failed = failure(1, before - Duration::hours(1));
        assert_eq!(next_step(&event, before, None, failed), Step::SkipLive);
        assert_eq!(next_step(&event, START, None, None), Step::Read);
    }

    #[test]
    fn a_running_event_is_read_again_once_its_readings_are_old() {
        let event = event(3);
        let refreshed = START + Duration::hours(1);
        let step = |after: Duration| next_step(&event, refreshed + after, Some(refreshed), None);
        assert_eq!(step(Duration::ZERO), Step::SkipFresh);
        assert_eq!(step(Duration::minutes(14)), Step::SkipFresh);
        assert_eq!(step(Duration::minutes(15)), Step::Read);
    }

    #[test]
    fn settlement_does_not_wait_for_provisional_readings_to_age() {
        let event = event(3);
        let completed = event.end_observation_date + Duration::minutes(1);
        assert_eq!(
            next_step(&event, completed, Some(event.end_observation_date), None),
            Step::SkipFresh
        );
        let refreshed = event.signing_date - Duration::minutes(1);
        assert_eq!(
            next_step(&event, event.signing_date, Some(refreshed), None),
            Step::Read
        );
    }

    #[test]
    fn a_failing_event_waits_out_its_failures() {
        let event = event(3);
        let failed_at = event.signing_date + Duration::minutes(1);
        let step = |count: u32, after: Duration| {
            next_step(&event, failed_at + after, None, failure(count, failed_at))
        };
        assert_eq!(step(1, Duration::minutes(14)), Step::SkipBackedOff);
        assert_eq!(step(1, Duration::minutes(15)), Step::Read);
        assert_eq!(step(2, Duration::minutes(29)), Step::SkipBackedOff);
        assert_eq!(step(2, Duration::minutes(30)), Step::Read);
        assert_eq!(step(9, Duration::minutes(359)), Step::SkipBackedOff);
        assert_eq!(step(9, Duration::hours(6)), Step::Read);
    }

    #[test]
    fn the_first_settlement_check_is_not_delayed_by_earlier_failures() {
        let event = event(3);
        let failed = failure(6, event.signing_date - Duration::minutes(5));
        let before = event.signing_date - Duration::minutes(1);
        assert_eq!(next_step(&event, before, None, failed), Step::SkipBackedOff);
        assert_eq!(
            next_step(&event, event.signing_date, None, failed),
            Step::Read
        );
    }

    #[test]
    fn an_event_without_entries_is_settled_at_its_signing_date() {
        let event = event(0);
        let before = event.signing_date - Duration::SECOND;
        assert_eq!(next_step(&event, before, None, None), Step::Read);
        // Nothing about its source can hold the event open.
        let failed = failure(3, event.signing_date);
        assert_eq!(
            next_step(&event, event.signing_date, None, failed),
            Step::SettleWithoutEntries
        );
    }
}
