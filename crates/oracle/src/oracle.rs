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
    database::{AwaitingAttestation, Database, EntryScore, SettlementOutcome, WriteError},
    events::{
        AWAITING_COLLECTION, AddEventEntry, BASELINE_UNAVAILABLE, CreateEvent,
        EXPIRY_AFTER_SIGNING, EntryRejection, Event, EventCounts, EventFilter, EventListQuery,
        EventRecord, EventRejection, EventStatus, EventSummary, NewEvent, SettlementBlock,
        WeatherEntry, validate_entries,
    },
    lines::{self, Line, LinePass, LineSettings},
    publication::{Publication, Stage},
    scoring::{self, NotUuidV7, PickRule, Scored, ScoringRules},
    signing::{AttestError, KeyError, SigningKey},
    sources::{ObservationWindow, OutcomeSource, PlannedBaseline, Reading, SourceError, Sources},
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
    #[error("the nostr publishing key is the oracle signing key; use a separate key file")]
    SharedPublishingKey,
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

/// How long a collection run takes to ask its source for every target. A
/// run that started this long before a signing date can still hold a
/// request made after it, which is what settlement needs.
const COLLECTION_RUN: Duration = Duration::minutes(5);

/// How long after its signing date an event's missing observations are
/// expected to arrive with the next collection. Until then a settlement
/// check waits for a new collection instead of a timer; a gap that outlasts
/// it is retried with the growing wait.
const COLLECTION_SETTLING: Duration = Duration::hours(2);

/// What one processing pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EtlSummary {
    /// Events whose processing failed; they are retried after a wait that
    /// grows with every failure.
    pub failed: usize,
    /// Events attested in this pass.
    pub attested: usize,
    /// Admitted events that reached expiry before finalization.
    pub expired: usize,
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
    /// Events left unread because they can never be attested.
    pub skipped_unsettleable: usize,
    /// Events past their signing date left unread until a new observation
    /// collection is published.
    pub awaiting_collection: usize,
}

/// A failed read of an event, and how many in a row it makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Failure {
    count: u32,
    at: OffsetDateTime,
    /// The published observations did not cover the window.
    coverage: bool,
    /// When the newest observation collection had started, if known.
    collection: Option<OffsetDateTime>,
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
    /// When the newest published observation collection started, as the
    /// process last told the oracle. `None` when it never did: settlement
    /// checks then run on timers alone.
    collection: Option<OffsetDateTime>,
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
    /// A scored baseline does not exist: no read can settle the event.
    SkipUnsettleable,
    /// The signing date passed and the observations that settle the event
    /// come with a collection that is not published yet.
    AwaitCollection,
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

/// [`next_step`] for an event whose settlement is due, given what is known
/// beyond its timers: whether its stored block is final, and when the newest
/// observation collection started.
///
/// Settlement needs a source request made after the signing date, so the
/// first check waits for a collection that can hold one, and a check that
/// found the observations short waits for the next collection. Both apply
/// for [`COLLECTION_SETTLING`] after the signing date and only while the
/// collection time is known; past that the timers of [`next_step`] decide.
fn settlement_step(
    event: &EventRecord,
    now: OffsetDateTime,
    refreshed: Option<OffsetDateTime>,
    failure: Option<Failure>,
    unsettleable: bool,
    collection: Option<OffsetDateTime>,
) -> Step {
    let step = next_step(event, now, refreshed, failure);
    if !signing_due(event, now) || event.total_entries == 0 {
        return step;
    }
    if unsettleable {
        return Step::SkipUnsettleable;
    }
    let (Some(collection), true) = (
        collection,
        now < event.signing_date.saturating_add(COLLECTION_SETTLING),
    ) else {
        return step;
    };
    // Failures from before the signing date say nothing about settlement.
    match failure.filter(|failure| failure.next_attempt(event, now).is_some()) {
        None if collection < event.signing_date.saturating_sub(COLLECTION_RUN) => {
            Step::AwaitCollection
        }
        None => step,
        Some(failure) if failure.coverage => {
            if failure.collection.is_none_or(|seen| collection > seen) {
                Step::Read
            } else {
                Step::AwaitCollection
            }
        }
        Some(_) => step,
    }
}

/// When a blocked event is read next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NextAttempt {
    At(OffsetDateTime),
    /// When the next observation collection is published.
    NextCollection,
    /// Never: the event cannot be attested and ends through its expiry.
    Never,
}

/// An event past its signing date that has entries and no attestation.
#[derive(Clone, Debug, PartialEq)]
pub struct OverdueEvent {
    pub id: Uuid,
    pub locations: Vec<String>,
    pub signing_date: OffsetDateTime,
    /// When its contract expires and the event can no longer be attested.
    pub expires_at: OffsetDateTime,
    pub total_entries: usize,
    /// Why the last check did not sign it, if a check has run.
    pub block: Option<SettlementBlock>,
    pub next_attempt: Option<NextAttempt>,
}

/// The `target/metric` pairs of `targets` and `metrics` that `planned` gives
/// no baseline, each with the source's reason. Nothing when the source
/// planned nothing: it cannot tell, and creation is not checked.
fn unplanned_baselines(
    targets: &[String],
    metrics: &[String],
    planned: &[PlannedBaseline],
) -> Vec<String> {
    if planned.is_empty() {
        return vec![];
    }
    let mut missing = vec![];
    for target in targets {
        for metric in metrics {
            let mut rows = planned
                .iter()
                .filter(|row| &row.target == target && &row.metric == metric);
            let row = rows.next().filter(|_| rows.next().is_none());
            if row.is_some_and(|row| row.reason.is_none() && row.value.is_some_and(f64::is_finite))
            {
                continue;
            }
            let reason = row
                .and_then(|row| row.reason.as_deref())
                .unwrap_or("the source has no forecast for it");
            missing.push(format!("{target}/{metric} ({reason})"));
        }
    }
    missing
}

pub struct Oracle {
    db: Database,
    sources: Sources,
    key: Arc<SigningKey>,
    clock: Clock,
    lines: LineSettings,
    etl: Mutex<EtlMemory>,
    /// Where announcements and attestations are queued for Nostr relays.
    publication: Option<Publication>,
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
            publication: None,
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

    /// Queues announcements and attestations for Nostr relays from now on.
    /// Refuses a publishing key equal to the attestation key.
    pub fn with_publication(mut self, publication: Publication) -> Result<Self, Error> {
        if publication.public_key.to_bytes() == self.key.x_only_public_key().serialize() {
            return Err(Error::SharedPublishingKey);
        }
        self.publication = Some(publication);
        Ok(self)
    }

    pub fn publication(&self) -> Option<&Publication> {
        self.publication.as_ref()
    }

    /// Queues `stage` of `event_id` for each relay, when publishing is on.
    /// Runs after the event write commits and never fails the caller: a
    /// row that cannot be queued now is queued by the publisher's next
    /// sweep of recent events.
    async fn queue_publication(&self, event_id: Uuid, stage: Stage) {
        let Some(publication) = &self.publication else {
            return;
        };
        match self
            .db
            .queue_publication(
                event_id,
                stage,
                publication.relays.clone(),
                self.now().unix_timestamp(),
            )
            .await
        {
            Ok(()) => publication.wake.notify_one(),
            Err(error) => warn!(
                "event {event_id}: cannot queue its {} publication yet: {error}",
                stage.as_str()
            ),
        }
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
        self.require_baselines(&new_event).await?;
        self.db.add_event(&new_event).await?;
        info!(
            "created event {} for {} with {} outcomes",
            new_event.id,
            new_event.coordinator_pubkey,
            new_event.event_announcement.locking_points.len()
        );
        self.queue_publication(new_event.id, Stage::Announced).await;
        self.get_event(new_event.id).await
    }

    /// Refuses an event when its source has no baseline for a target and
    /// metric it scores. Such an event could collect entries and then never
    /// be attested, leaving its contract to run out.
    async fn require_baselines(&self, new_event: &NewEvent) -> Result<(), Error> {
        let source = self
            .sources
            .get(&new_event.source)
            .ok_or_else(|| Error::UnknownSource {
                event: new_event.id,
                source_id: new_event.source.clone(),
            })?;
        let window = ObservationWindow {
            start: new_event.start_observation_date,
            end: new_event.end_observation_date,
        };
        let planned = source
            .planned_baselines(window, &new_event.locations)
            .await?;
        let missing = unplanned_baselines(&new_event.locations, &new_event.metrics, &planned);
        if missing.is_empty() {
            Ok(())
        } else {
            Err(EventRejection::BaselineUnavailable(missing.join(", ")).into())
        }
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
                Step::SkipUnsettleable => {
                    summary.skipped_unsettleable += 1;
                    continue;
                }
                Step::AwaitCollection => {
                    summary.awaiting_collection += 1;
                    self.note_awaiting_collection(&event, blocks.get(&id)).await;
                    continue;
                }
                Step::SettleWithoutEntries => {
                    let settled = self.settle_without_entries(&event).await;
                    if matches!(settled, Ok(true)) {
                        summary.settled_without_entries += 1;
                    }
                    settled.map(|_| SettlementOutcome::Unchanged)
                }
                Step::Read => {
                    summary.refreshed += 1;
                    self.process_event(event).await
                }
            };
            match result {
                Ok(SettlementOutcome::Attested) => summary.attested += 1,
                Ok(SettlementOutcome::Expired) => summary.expired += 1,
                Ok(SettlementOutcome::Unchanged) => {}
                Err(error) => {
                    summary.failed += 1;
                    error!("etl {etl_process_id}: event {id} failed: {error:#}");
                }
            }
            // Each read allocates hundreds of megabytes that glibc would
            // otherwise keep: hand them back before the next event.
            crate::heavy::release_freed_memory();
        }
        info!(
            "etl {etl_process_id}: done, {} attested, {} failed, {} refreshed, \
             {} skipped as live, {} skipped as backed off, {} skipped as recently refreshed, \
             {} settled without entries, {} expired before finalization, \
             {} awaiting a collection, {} unsettleable",
            summary.attested,
            summary.failed,
            summary.refreshed,
            summary.skipped_live,
            summary.skipped_backed_off,
            summary.skipped_fresh,
            summary.settled_without_entries,
            summary.expired,
            summary.awaiting_collection,
            summary.skipped_unsettleable
        );
        Ok(summary)
    }

    /// Tells the oracle when the newest published observation collection
    /// started, so settlement checks can wait for the collection they need
    /// (see [`settlement_step`]). Called before each pass by a process that
    /// knows; without it checks run on timers alone.
    pub fn set_latest_collection(&self, started: Option<OffsetDateTime>) {
        self.etl_memory().collection = started;
    }

    /// Records why an event past its signing date was left unread, unless a
    /// settlement check already recorded its own reason.
    async fn note_awaiting_collection(&self, event: &EventRecord, block: Option<&SettlementBlock>) {
        if block.is_some_and(|block| block.checked_at >= event.signing_date) {
            return;
        }
        let block = SettlementBlock {
            code: AWAITING_COLLECTION.into(),
            message: "the observation collection that checks the end of the window has not \
                      been published yet"
                .into(),
            checked_at: self.now(),
        };
        if let Err(error) = self.db.set_settlement_block(event.id, block, true).await {
            warn!(
                "event {}: cannot record that it awaits a collection: {error:#}",
                event.id
            );
        }
    }

    /// When `block`, the stored block of the unsigned event `id`, lets the
    /// event be read again.
    pub fn next_attempt(
        &self,
        id: Uuid,
        signing_date: OffsetDateTime,
        block: &SettlementBlock,
    ) -> NextAttempt {
        if block.is_final() {
            return NextAttempt::Never;
        }
        if block.code == AWAITING_COLLECTION {
            return NextAttempt::NextCollection;
        }
        let now = self.now();
        let memory = self.etl_memory();
        match memory.failures.get(&id) {
            Some(failure)
                if failure.coverage
                    && memory.collection.is_some()
                    && now < signing_date.saturating_add(COLLECTION_SETTLING) =>
            {
                NextAttempt::NextCollection
            }
            Some(failure) => NextAttempt::At(failure.at + retry_delay(failure.count)),
            None => NextAttempt::At(block.checked_at + retry_delay(1)),
        }
    }

    /// Events with entries whose signing date has passed without an
    /// attestation, oldest signing date first, with why and when each is
    /// read next. Events past their expiry are left out.
    pub async fn overdue_events(&self) -> Result<Vec<OverdueEvent>, Error> {
        let now = self.now();
        let mut events: Vec<EventRecord> = self
            .db
            .events_to_settle(now)
            .await?
            .into_iter()
            .filter(|event| signing_due(event, now) && event.total_entries > 0)
            .collect();
        events.sort_by_key(|event| event.signing_date);
        let ids: Vec<Uuid> = events.iter().map(|event| event.id).collect();
        let mut blocks = self.db.settlement_blocks(&ids).await?;
        Ok(events
            .into_iter()
            .map(|event| {
                let block = blocks.remove(&event.id);
                OverdueEvent {
                    next_attempt: block
                        .as_ref()
                        .map(|block| self.next_attempt(event.id, event.signing_date, block)),
                    id: event.id,
                    locations: event.locations,
                    signing_date: event.signing_date,
                    expires_at: event.signing_date.saturating_add(EXPIRY_AFTER_SIGNING),
                    total_entries: event.total_entries,
                    block,
                }
            })
            .collect())
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
    /// stored `block` counts as the event's first, unless it only records
    /// that the event waited for a collection.
    fn step_for(&self, event: &EventRecord, block: Option<&SettlementBlock>) -> Step {
        let now = self.now();
        let memory = self.etl_memory();
        let failure = memory.failures.get(&event.id).copied().or_else(|| {
            block
                .filter(|block| block.code != AWAITING_COLLECTION)
                .map(|block| Failure {
                    count: 1,
                    at: block.checked_at,
                    coverage: false,
                    collection: None,
                })
        });
        settlement_step(
            event,
            now,
            memory.refreshed.get(&event.id).copied(),
            failure,
            block.is_some_and(SettlementBlock::is_final),
            memory.collection,
        )
    }

    /// Counts a failed read at `now` and returns it. Failures from before
    /// the signing date do not lengthen the wait between settlement checks.
    /// `coverage` says the published observations did not cover the window.
    fn remember_failure(
        &self,
        event: &EventRecord,
        now: OffsetDateTime,
        coverage: bool,
    ) -> Failure {
        let mut memory = self.etl_memory();
        let earlier = memory
            .failures
            .get(&event.id)
            .filter(|failure| failure.next_attempt(event, now).is_some())
            .map_or(0, |failure| failure.count);
        let failure = Failure {
            count: earlier.saturating_add(1),
            at: now,
            coverage,
            collection: memory.collection,
        };
        memory.failures.insert(event.id, failure);
        failure
    }

    fn remember_refresh(&self, event: Uuid, now: OffsetDateTime) {
        let mut memory = self.etl_memory();
        memory.failures.remove(&event);
        memory.refreshed.insert(event, now);
    }

    /// Reports attestation, expiry, or a provisional refresh.
    async fn process_event(&self, event: EventRecord) -> Result<SettlementOutcome, Error> {
        let result = self.refresh_event(&event).await;
        let now = self.now();
        match &result {
            Ok(_) => self.remember_refresh(event.id, now),
            Err(error) => {
                let source_coverage = matches!(error, Error::Source(SourceError::Coverage(_)));
                let failure = self.remember_failure(&event, now, source_coverage);
                let code = match error {
                    Error::Source(SourceError::DataQuality { .. }) => "data_quality",
                    Error::Source(SourceError::SettlementBlocked(_)) => "incomplete_readings",
                    Error::Source(SourceError::BaselineUnavailable(_)) => BASELINE_UNAVAILABLE,
                    Error::Source(_) => "source_unavailable",
                    _ => "processing_failed",
                };
                if code == BASELINE_UNAVAILABLE {
                    warn!(
                        "event {} cannot be attested and will not be read again: {error}",
                        event.id
                    );
                } else {
                    warn!(
                        "event {}: failure {} in a row; next attempt at {}",
                        event.id,
                        failure.count,
                        now + retry_delay(failure.count)
                    );
                }
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

    async fn refresh_event(&self, event: &EventRecord) -> Result<SettlementOutcome, Error> {
        if self.now() >= event.signing_date.saturating_add(EXPIRY_AFTER_SIGNING) {
            return Ok(SettlementOutcome::Expired);
        }
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
        Ok(SettlementOutcome::Unchanged)
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
    /// Returns the finalization outcome after the writer rechecks expiry.
    async fn attest(
        &self,
        event: &EventRecord,
        scored: &[Scored],
        readings: Vec<Reading>,
    ) -> Result<SettlementOutcome, Error> {
        let winners = scoring::winning_indices(scored, event.number_of_places_win);
        let message = scoring::outcome_message(&winners);
        let announcement = self
            .db
            .event_announcement(event.id)
            .await?
            .ok_or(Error::EventNotFound(event.id))?;
        if self.now() >= event.signing_date.saturating_add(EXPIRY_AFTER_SIGNING) {
            return Ok(SettlementOutcome::Expired);
        }
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
            .settle_event(
                event.id,
                readings,
                entry_scores(scored),
                attestation,
                self.clock.clone(),
            )
            .await?;
        match stored {
            SettlementOutcome::Attested => {
                info!("attested event {} with winners {winners:?}", event.id);
                self.queue_publication(event.id, Stage::Attested).await;
            }
            SettlementOutcome::Unchanged => {
                info!("event {} already finalized; kept its attestation", event.id);
            }
            SettlementOutcome::Expired => {
                info!("event {} reached expiry before finalization", event.id);
            }
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
        Some(Failure {
            count,
            at,
            coverage: false,
            collection: None,
        })
    }

    /// A failed check that found the observations short of the window while
    /// the newest collection had started at `collection`.
    fn coverage_failure(at: OffsetDateTime, collection: OffsetDateTime) -> Option<Failure> {
        Some(Failure {
            count: 1,
            at,
            coverage: true,
            collection: Some(collection),
        })
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
    fn the_first_settlement_check_waits_for_a_collection_that_can_cover_it() {
        let event = event(3);
        let signing = event.signing_date;
        let step = |now: OffsetDateTime, collection: Option<OffsetDateTime>| {
            settlement_step(&event, now, None, None, false, collection)
        };
        // The newest collection started well before the signing date: no
        // request in it was made after that date, so the check would fail.
        let earlier = signing - Duration::minutes(40);
        assert_eq!(
            step(signing + Duration::minutes(1), Some(earlier)),
            Step::AwaitCollection
        );
        // A run that started just before the signing date can hold one.
        assert_eq!(
            step(
                signing + Duration::minutes(8),
                Some(signing - COLLECTION_RUN)
            ),
            Step::Read
        );
        // The next collection is published: the first check runs then.
        let covering = signing + Duration::minutes(20);
        assert_eq!(
            step(covering + Duration::minutes(9), Some(covering)),
            Step::Read
        );
        // Without a known collection time, and once the settling time is
        // over, the timers decide as before.
        assert_eq!(step(signing + Duration::minutes(1), None), Step::Read);
        assert_eq!(
            step(signing + COLLECTION_SETTLING, Some(earlier)),
            Step::Read
        );
        // Before the signing date nothing changes.
        let before = signing - Duration::minutes(1);
        assert_eq!(step(before, Some(earlier)), Step::Read);
    }

    #[test]
    fn a_check_short_of_observations_runs_again_with_the_next_collection() {
        let event = event(3);
        let signing = event.signing_date;
        let seen = signing - Duration::minutes(3);
        let failed_at = signing + Duration::minutes(6);
        let step = |now: OffsetDateTime, collection: OffsetDateTime| {
            settlement_step(
                &event,
                now,
                None,
                coverage_failure(failed_at, seen),
                false,
                Some(collection),
            )
        };
        // The same collection cannot answer differently, timer or not.
        assert_eq!(
            step(failed_at + Duration::minutes(5), seen),
            Step::AwaitCollection
        );
        assert_eq!(
            step(failed_at + Duration::minutes(40), seen),
            Step::AwaitCollection
        );
        // A new one is read at once, before the fifteen-minute wait is over.
        let next = seen + Duration::hours(1);
        assert_eq!(step(next + Duration::minutes(9), next), Step::Read);
        // A gap that outlasts the settling time goes back to the timers.
        let late = signing + COLLECTION_SETTLING;
        assert_eq!(step(late, seen), Step::Read);
        let again = Some(Failure {
            count: 4,
            at: late,
            coverage: true,
            collection: Some(seen),
        });
        assert_eq!(
            settlement_step(
                &event,
                late + Duration::minutes(119),
                None,
                again,
                false,
                Some(seen)
            ),
            Step::SkipBackedOff
        );
        assert_eq!(
            settlement_step(
                &event,
                late + Duration::hours(2),
                None,
                again,
                false,
                Some(seen)
            ),
            Step::Read
        );
        // Other failures keep their timers while a collection is awaited.
        let other = failure(1, failed_at);
        assert_eq!(
            settlement_step(
                &event,
                failed_at + Duration::minutes(5),
                None,
                other,
                false,
                Some(next)
            ),
            Step::SkipBackedOff
        );
    }

    #[test]
    fn an_event_without_a_baseline_is_never_read_again() {
        let event = event(3);
        let signing = event.signing_date;
        let failed = failure(1, signing);
        for after in [Duration::ZERO, Duration::hours(7), Duration::hours(23)] {
            assert_eq!(
                settlement_step(&event, signing + after, None, failed, true, None),
                Step::SkipUnsettleable
            );
        }
        // Without entries there is no outcome to hold: it is closed as usual.
        let empty = self::event(0);
        assert_eq!(
            settlement_step(&empty, empty.signing_date, None, None, true, None),
            Step::SettleWithoutEntries
        );
    }

    #[test]
    fn creation_names_every_pair_without_a_baseline() {
        let planned = |target: &str, metric: &str, value: Option<f64>, reason: Option<&str>| {
            PlannedBaseline {
                target: target.into(),
                metric: metric.into(),
                value,
                reason: reason.map(str::to_owned),
            }
        };
        let targets = vec!["KORD".to_owned(), "PAGK".to_owned()];
        let metrics = vec!["temp_high".to_owned(), "wind_speed".to_owned()];
        let rows = vec![
            planned("KORD", "temp_high", Some(70.0), None),
            planned("KORD", "wind_speed", Some(9.0), None),
            planned("KORD", "rain_amt", None, Some("not scored")),
            planned("PAGK", "temp_high", Some(37.0), None),
            planned(
                "PAGK",
                "wind_speed",
                None,
                Some("native forecast value is missing"),
            ),
        ];
        assert_eq!(
            unplanned_baselines(&targets, &metrics, &rows),
            vec!["PAGK/wind_speed (native forecast value is missing)"]
        );
        // A pair the source did not assess, or assessed twice, has none.
        let mut twice = rows.clone();
        twice.push(planned("KORD", "temp_high", Some(71.0), None));
        twice.retain(|row| !(row.target == "PAGK" && row.metric == "temp_high"));
        assert_eq!(
            unplanned_baselines(&targets, &metrics, &twice),
            vec![
                "KORD/temp_high (the source has no forecast for it)",
                "PAGK/temp_high (the source has no forecast for it)",
                "PAGK/wind_speed (native forecast value is missing)",
            ]
        );
        let not_finite = vec![planned("KORD", "temp_high", Some(f64::NAN), None)];
        assert_eq!(
            unplanned_baselines(&targets[..1], &metrics[..1], &not_finite),
            vec!["KORD/temp_high (the source has no forecast for it)"]
        );
        // A source that plans nothing cannot tell: nothing is refused.
        assert!(unplanned_baselines(&targets, &metrics, &[]).is_empty());
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
