//! Events: the wire contract and the validated domain forms.
//!
//! The contract is source independent. An event names a data `source`, the
//! `locations` (targets) it watches, and the metrics (`scoring_fields`) it
//! scores. Entries are lists of [`Pick`]s: `Over`, `Par`, or `Under` the
//! source's baseline for a `(target, metric)`. The oracle attests the
//! ranking of entries, so the attestation does not depend on the data; see
//! `docs/attestation.md`.
//!
//! NOAA-shaped fields remain for existing clients: entries may send
//! [`WeatherChoices`] instead of picks, and responses carry `weather` rows
//! and `expected_observations` for NOAA events. Both are converted at this
//! boundary, so scoring and storage only see picks and readings. Field
//! names, optionality, and RFC 3339 dates stay stable; the contract test
//! posts the coordinator's exact JSON.
//!
//! Every rule for a new event or its entries lives here, in
//! [`NewEvent::build`] and [`validate_entries`].

use dlctix::{
    EventLockingConditions,
    secp::{MaybeScalar, Point},
};
use nostr::{key::PublicKey as NostrPublicKey, nips::nip19::ToBech32};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use time::{Duration, OffsetDateTime, UtcOffset};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    scoring::{self, Pick},
    signing::{EventNonce, SigningKey},
    sources::{OutcomeSource, Reading, SourceError, Sources, noaa},
};

/// Entries an event may hold. Outcome enumeration grows as
/// `entries! / (entries - places)!`, so this and [`MAX_OUTCOMES`] bound the
/// work and storage one event can cost.
pub const MAX_ENTRIES: usize = 25;
pub const MAX_PLACES: usize = 5;
/// Announced outcomes per event (25 entries with 3 places is 13,801).
pub const MAX_OUTCOMES: usize = 20_000;
pub const MAX_LOCATIONS: usize = 50;
pub const MAX_LIST_LIMIT: usize = 100;
/// Participants can claim a refund this long after the signing date if the
/// oracle has not attested.
const EXPIRY_AFTER_SIGNING: Duration = Duration::DAY;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateEvent {
    /// Client needs to provide a valid Uuidv7
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    /// Time at which the attestation will be added to the event, needs to be after the end observation date
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations start, must be before the end observation date
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations end; entries must be submitted before this time
    pub end_observation_date: OffsetDateTime,
    /// Data source to attest (see `GET /oracle/sources`); defaults to `noaa_weather`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Targets the event watches: NOAA station ids for `noaa_weather`
    #[serde(alias = "targets")]
    pub locations: Vec<String>,
    /// The number of picks each entry may make
    pub number_of_values_per_entry: usize,
    /// Total number of allowed entries into the event (at most 25)
    pub total_allowed_entries: usize,
    /// Number of ranks that win (1 to 5, fewer than the number of entries)
    pub number_of_places_win: usize,
    /// Metric ids to score, from the source's metric list. Defaults to the
    /// source's defaults (`temp_high`, `temp_low`, `wind_speed` for NOAA).
    #[serde(default, alias = "metrics", skip_serializing_if = "Option::is_none")]
    pub scoring_fields: Option<Vec<String>>,
    /// Keep the event off the oracle's events list and dashboard counts
    /// unless the reader asks to see unlisted events. It stays reachable by
    /// its id, on its own page and over the API. Defaults to `false`.
    #[serde(default)]
    pub unlisted: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum EventRejection {
    #[error("event id {0} is not a UUIDv7")]
    NotUuidV7(Uuid),
    #[error("unknown source {0:?}; see GET /oracle/sources")]
    UnknownSource(String),
    #[error("observation start must be before its end, and the end no later than the signing date")]
    DatesOutOfOrder,
    #[error("signing date is outside the DLC expiry range")]
    ExpiryOutOfRange,
    #[error("total_allowed_entries must be between 2 and {MAX_ENTRIES}, requested {0}")]
    Entries(usize),
    #[error(
        "number_of_places_win must be between 1 and {MAX_PLACES} and fewer than the entries, requested {0}"
    )]
    Places(usize),
    #[error(
        "{entries} entries with {places} places produce too many outcomes (max {MAX_OUTCOMES})"
    )]
    TooManyOutcomes { entries: usize, places: usize },
    #[error("an event needs between 1 and {MAX_LOCATIONS} distinct locations")]
    Locations,
    #[error(transparent)]
    InvalidLocation(SourceError),
    #[error("scoring fields must be distinct metrics of the source, at least one")]
    ScoringFields,
    #[error("number_of_values_per_entry must be between 1 and {max}, requested {requested}")]
    ValuesPerEntry { requested: usize, max: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum EntryRejection {
    #[error("entry id {0} is not a UUIDv7")]
    NotUuidV7(Uuid),
    #[error("entry {entry} belongs to event {actual}, not {expected}")]
    WrongEvent {
        entry: Uuid,
        expected: Uuid,
        actual: Uuid,
    },
    #[error("entry id {0} appears more than once")]
    DuplicateEntry(Uuid),
    #[error("the event needs exactly {expected} entries, received {received}")]
    Count { expected: usize, received: usize },
    #[error("entries are only accepted before the observation window ends")]
    Closed,
    #[error("entries were already submitted for this event")]
    AlreadySubmitted,
    #[error("entry {0} must make between 1 and {1} picks")]
    PickCount(Uuid, usize),
    #[error("entry {0} must use exactly one of `picks` or `expected_observations`")]
    PickFormat(Uuid),
    #[error(
        "entry {entry} uses `expected_observations`, which only {} events accept",
        noaa::NOAA_WEATHER
    )]
    WeatherChoicesUnsupported { entry: Uuid },
    #[error("entry {entry} picks location {location:?}, which is not in the event")]
    UnknownLocation { entry: Uuid, location: String },
    #[error("entry {entry} picks {field}, which the event does not score")]
    UnscoredField { entry: Uuid, field: String },
    #[error("entry {entry} picks {field} at {location:?} twice")]
    DuplicatePick {
        entry: Uuid,
        location: String,
        field: String,
    },
}

/// A validated event with its announcement, ready to store.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub id: Uuid,
    pub source: String,
    pub signing_date: OffsetDateTime,
    pub start_observation_date: OffsetDateTime,
    pub end_observation_date: OffsetDateTime,
    pub locations: Vec<String>,
    pub metrics: Vec<String>,
    pub number_of_values_per_entry: usize,
    pub total_allowed_entries: usize,
    pub number_of_places_win: usize,
    pub nonce: EventNonce,
    pub event_announcement: EventLockingConditions,
    /// The coordinator's npub.
    pub coordinator_pubkey: String,
    pub unlisted: bool,
}

impl NewEvent {
    /// Validates `event` for its source and computes its announcement: one
    /// locking point per ranking outcome plus the refund-all outcome, in
    /// the order the coordinator also generates. CPU-bound; call it off the
    /// async runtime.
    pub fn build(
        event: CreateEvent,
        sources: &Sources,
        key: &SigningKey,
        coordinator: NostrPublicKey,
    ) -> Result<Self, EventRejection> {
        if event.id.get_version_num() != 7 {
            return Err(EventRejection::NotUuidV7(event.id));
        }
        let source: &dyn OutcomeSource = match &event.source {
            Some(id) => sources
                .get(id)
                .ok_or_else(|| EventRejection::UnknownSource(id.clone()))?
                .as_ref(),
            None => sources.default_source().as_ref(),
        };
        if event.start_observation_date >= event.end_observation_date
            || event.end_observation_date > event.signing_date
        {
            return Err(EventRejection::DatesOutOfOrder);
        }
        let entries = event.total_allowed_entries;
        let places = event.number_of_places_win;
        if !(2..=MAX_ENTRIES).contains(&entries) {
            return Err(EventRejection::Entries(entries));
        }
        // Places equal to entries would collide with the refund-all outcome.
        if places == 0 || places > MAX_PLACES || places >= entries {
            return Err(EventRejection::Places(places));
        }
        if scoring::outcome_count(entries, places).is_none_or(|count| count > MAX_OUTCOMES) {
            return Err(EventRejection::TooManyOutcomes { entries, places });
        }
        let distinct_locations: HashSet<&String> = event.locations.iter().collect();
        if event.locations.is_empty()
            || event.locations.len() > MAX_LOCATIONS
            || distinct_locations.len() != event.locations.len()
        {
            return Err(EventRejection::Locations);
        }
        for location in &event.locations {
            source
                .validate_target(location)
                .map_err(EventRejection::InvalidLocation)?;
        }
        let metrics: Vec<String> = match event.scoring_fields {
            Some(fields) => fields,
            None => source
                .default_metrics()
                .iter()
                .map(|metric| (*metric).to_owned())
                .collect(),
        };
        let distinct_metrics: HashSet<&String> = metrics.iter().collect();
        if metrics.is_empty()
            || distinct_metrics.len() != metrics.len()
            || metrics.iter().any(|metric| source.metric(metric).is_none())
        {
            return Err(EventRejection::ScoringFields);
        }
        let max_values = event.locations.len() * metrics.len();
        if !(1..=max_values).contains(&event.number_of_values_per_entry) {
            return Err(EventRejection::ValuesPerEntry {
                requested: event.number_of_values_per_entry,
                max: max_values,
            });
        }
        let signing_date = event.signing_date.to_offset(UtcOffset::UTC);
        let expiry = u32::try_from(
            signing_date
                .checked_add(EXPIRY_AFTER_SIGNING)
                .ok_or(EventRejection::ExpiryOutOfRange)?
                .unix_timestamp(),
        )
        .map_err(|_| EventRejection::ExpiryOutOfRange)?;

        let nonce = key.new_event_nonce(event.id);
        let locking_points = scoring::ranking_outcomes(entries, places)
            .iter()
            .map(|winners| key.locking_point(nonce.point, &scoring::outcome_message(winners)))
            .collect();
        let Ok(coordinator_pubkey) = coordinator.to_bech32();
        Ok(Self {
            id: event.id,
            source: source.id().as_str().to_owned(),
            signing_date,
            start_observation_date: event.start_observation_date.to_offset(UtcOffset::UTC),
            end_observation_date: event.end_observation_date.to_offset(UtcOffset::UTC),
            locations: event.locations,
            metrics,
            number_of_values_per_entry: event.number_of_values_per_entry,
            total_allowed_entries: entries,
            number_of_places_win: places,
            nonce,
            event_announcement: EventLockingConditions {
                expiry: Some(expiry),
                locking_points,
            },
            coordinator_pubkey,
            unlisted: event.unlisted,
        })
    }
}

/// A stored event without its (large) announcement.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRecord {
    pub id: Uuid,
    pub source: String,
    pub signing_date: OffsetDateTime,
    pub start_observation_date: OffsetDateTime,
    pub end_observation_date: OffsetDateTime,
    pub locations: Vec<String>,
    pub metrics: Vec<String>,
    pub number_of_values_per_entry: usize,
    pub total_allowed_entries: usize,
    pub number_of_places_win: usize,
    pub nonce: EventNonce,
    pub coordinator_pubkey: String,
    pub attestation: Option<MaybeScalar>,
    pub total_entries: usize,
    pub unlisted: bool,
}

impl EventRecord {
    pub fn status(&self, now: OffsetDateTime) -> EventStatus {
        if self.attestation.is_some() {
            EventStatus::Signed
        } else if now < self.start_observation_date {
            EventStatus::Live
        } else if now < self.end_observation_date {
            EventStatus::Running
        } else {
            EventStatus::Completed
        }
    }

    fn is_noaa(&self) -> bool {
        self.source == noaa::NOAA_WEATHER.as_str()
    }

    /// NOAA display rows; empty for other sources.
    fn weather(&self, readings: &[Reading]) -> Vec<Weather> {
        if self.is_noaa() {
            weather_from_readings(&self.locations, readings, self.start_observation_date)
        } else {
            vec![]
        }
    }

    pub fn into_event(
        self,
        now: OffsetDateTime,
        event_announcement: EventLockingConditions,
        entries: Vec<Entry>,
        readings: &[Reading],
    ) -> Event {
        Event {
            id: self.id,
            status: self.status(now),
            weather: self.weather(readings),
            readings: readings.to_vec(),
            signing_date: self.signing_date,
            start_observation_date: self.start_observation_date,
            end_observation_date: self.end_observation_date,
            number_of_values_per_entry: wire_count(self.number_of_values_per_entry),
            total_allowed_entries: wire_count(self.total_allowed_entries),
            number_of_places_win: wire_count(self.number_of_places_win),
            entry_ids: entries.iter().map(|entry| entry.id).collect(),
            entries: entries.into_iter().map(WeatherEntry::from).collect(),
            nonce_point: self.nonce.point,
            event_announcement,
            attestation: self.attestation,
            coordinator_pubkey: self.coordinator_pubkey,
            scoring_fields: self.metrics,
            source: self.source,
            locations: self.locations,
            unlisted: self.unlisted,
        }
    }

    pub fn into_summary(self, now: OffsetDateTime, readings: &[Reading]) -> EventSummary {
        EventSummary {
            id: self.id,
            status: self.status(now),
            weather: self.weather(readings),
            readings: readings.to_vec(),
            signing_date: self.signing_date,
            start_observation_date: self.start_observation_date,
            end_observation_date: self.end_observation_date,
            number_of_values_per_entry: wire_count(self.number_of_values_per_entry),
            total_allowed_entries: wire_count(self.total_allowed_entries),
            total_entries: wire_count(self.total_entries),
            number_of_places_win: wire_count(self.number_of_places_win),
            attestation: self.attestation,
            nonce_point: self.nonce.point,
            scoring_fields: self.metrics,
            source: self.source,
            locations: self.locations,
            unlisted: self.unlisted,
        }
    }
}

/// Counts are bounded far below `i64::MAX` by validation.
fn wire_count(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// A stored entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: Uuid,
    pub event_id: Uuid,
    pub picks: Vec<Pick>,
    pub score: Option<i64>,
    pub base_score: Option<i64>,
}

/// Checks a full set of entries against `event` and converts them to picks.
/// The coordinator submits every entry at once, before the window ends.
pub fn validate_entries(
    event: &EventRecord,
    entries: Vec<AddEventEntry>,
    now: OffsetDateTime,
) -> Result<Vec<Entry>, EntryRejection> {
    if now >= event.end_observation_date {
        return Err(EntryRejection::Closed);
    }
    if event.total_entries > 0 {
        return Err(EntryRejection::AlreadySubmitted);
    }
    if entries.len() != event.total_allowed_entries {
        return Err(EntryRejection::Count {
            expected: event.total_allowed_entries,
            received: entries.len(),
        });
    }
    let mut ids = HashSet::new();
    entries
        .into_iter()
        .map(|entry| {
            if entry.id.get_version_num() != 7 {
                return Err(EntryRejection::NotUuidV7(entry.id));
            }
            if entry.event_id != event.id {
                return Err(EntryRejection::WrongEvent {
                    entry: entry.id,
                    expected: event.id,
                    actual: entry.event_id,
                });
            }
            if !ids.insert(entry.id) {
                return Err(EntryRejection::DuplicateEntry(entry.id));
            }
            let picks = entry_picks(event, &entry)?;
            Ok(Entry {
                id: entry.id,
                event_id: entry.event_id,
                picks,
                score: None,
                base_score: None,
            })
        })
        .collect()
}

/// The entry's picks, from whichever form it used, checked against the
/// event: known targets, enabled metrics, no repeats, and a bounded count.
fn entry_picks(event: &EventRecord, entry: &AddEventEntry) -> Result<Vec<Pick>, EntryRejection> {
    let picks: Vec<Pick> = match (
        entry.picks.is_empty(),
        entry.expected_observations.is_empty(),
    ) {
        (false, true) => entry.picks.clone(),
        (true, false) if event.is_noaa() => entry
            .expected_observations
            .iter()
            .flat_map(|choice| {
                choice.predictions().map(|(field, prediction)| Pick {
                    target: choice.stations.clone(),
                    metric: field.as_str().to_owned(),
                    prediction,
                })
            })
            .collect(),
        (true, false) => return Err(EntryRejection::WeatherChoicesUnsupported { entry: entry.id }),
        (true, true) => vec![],
        (false, false) => return Err(EntryRejection::PickFormat(entry.id)),
    };
    for (index, pick) in picks.iter().enumerate() {
        if !event.locations.contains(&pick.target) {
            return Err(EntryRejection::UnknownLocation {
                entry: entry.id,
                location: pick.target.clone(),
            });
        }
        if !event.metrics.contains(&pick.metric) {
            return Err(EntryRejection::UnscoredField {
                entry: entry.id,
                field: pick.metric.clone(),
            });
        }
        if picks[..index]
            .iter()
            .any(|earlier| earlier.target == pick.target && earlier.metric == pick.metric)
        {
            return Err(EntryRejection::DuplicatePick {
                entry: entry.id,
                location: pick.target.clone(),
                field: pick.metric.clone(),
            });
        }
    }
    if picks.is_empty() || picks.len() > event.number_of_values_per_entry {
        return Err(EntryRejection::PickCount(
            entry.id,
            event.number_of_values_per_entry,
        ));
    }
    Ok(picks)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, IntoParams)]
pub struct EventFilter {
    /// At most 100 events; defaults to 100.
    pub limit: Option<usize>,
    /// Comma separated event ids (at most 100).
    pub event_ids: Option<String>,
}

impl EventFilter {
    pub fn limit(&self) -> usize {
        self.limit
            .unwrap_or(MAX_LIST_LIMIT)
            .clamp(1, MAX_LIST_LIMIT)
    }

    /// Parsed ids; unparseable ids are an error rather than ignored.
    pub fn event_ids(&self) -> Result<Vec<Uuid>, uuid::Error> {
        self.event_ids
            .iter()
            .flat_map(|ids| ids.split(','))
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .take(MAX_LIST_LIMIT)
            .map(Uuid::parse_str)
            .collect()
    }
}

/// One page of the events list. The database applies every filter before
/// the limit, so older listed events are never crowded out by newer
/// unlisted ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventListQuery {
    pub status: Option<EventStatus>,
    pub include_unlisted: bool,
    /// Only events created before this one; ids grow with creation time.
    pub before: Option<Uuid>,
    pub limit: usize,
}

/// Events by status, counted with the same unlisted setting as the list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventCounts {
    pub live: usize,
    pub running: usize,
    pub completed: usize,
    pub signed: usize,
    /// Unlisted events, whether or not they are counted above.
    pub unlisted: usize,
}

impl EventCounts {
    pub fn of(&self, status: Option<EventStatus>) -> usize {
        match status {
            None => self.live + self.running + self.completed + self.signed,
            Some(EventStatus::Live) => self.live,
            Some(EventStatus::Running) => self.running,
            Some(EventStatus::Completed) => self.completed,
            Some(EventStatus::Signed) => self.signed,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub enum EventStatus {
    /// Observation window has not started; entries can be added
    #[default]
    Live,
    /// Inside the observation window
    Running,
    /// Observation window has finished, not yet signed
    Completed,
    /// Signed by the oracle
    Signed,
}

impl EventStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Signed => "signed",
        }
    }
}

impl std::fmt::Display for EventStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct EventSummary {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end_observation_date: OffsetDateTime,
    /// NOAA observation stations used in this event
    pub locations: Vec<String>,
    pub number_of_values_per_entry: i64,
    pub status: EventStatus,
    pub total_allowed_entries: i64,
    pub total_entries: i64,
    pub number_of_places_win: i64,
    /// Data source the event attests to
    pub source: String,
    /// Metric ids the event scores
    pub scoring_fields: Vec<String>,
    /// Latest baseline and observed value per target and metric
    pub readings: Vec<Reading>,
    /// NOAA display rows (empty for other sources)
    pub weather: Vec<Weather>,
    /// Present once the oracle has attested the final result
    #[schema(value_type = Option<String>)]
    pub attestation: Option<MaybeScalar>,
    /// Public nonce point `R` the attestation is made with
    #[schema(value_type = String)]
    pub nonce_point: Point,
    /// Left off the oracle's events list unless asked for
    #[serde(default)]
    pub unlisted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct Event {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    /// Time at which the attestation will be added to the event
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end_observation_date: OffsetDateTime,
    /// NOAA observation stations used in this event
    pub locations: Vec<String>,
    pub number_of_values_per_entry: i64,
    pub status: EventStatus,
    pub total_allowed_entries: i64,
    pub entry_ids: Vec<Uuid>,
    pub number_of_places_win: i64,
    /// All entries, in id order; entry `i` is outcome index `i`
    pub entries: Vec<WeatherEntry>,
    /// Data source the event attests to
    pub source: String,
    /// Latest baseline and observed value per target and metric
    pub readings: Vec<Reading>,
    /// NOAA display rows (empty for other sources)
    pub weather: Vec<Weather>,
    /// Public nonce point `R`. Locking point `i` is
    /// `R + H(R, P, outcome_i)·P` for the oracle key `P`.
    #[schema(value_type = String)]
    pub nonce_point: Point,
    /// The outcomes the oracle will attest to
    #[schema(value_type = Object)]
    pub event_announcement: EventLockingConditions,
    /// Present once the oracle has attested the final result
    #[schema(value_type = Option<String>)]
    pub attestation: Option<MaybeScalar>,
    /// The coordinator's npub
    pub coordinator_pubkey: String,
    /// Metric ids the event scores
    pub scoring_fields: Vec<String>,
    /// Left off the oracle's events list unless asked for
    #[serde(default)]
    pub unlisted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Weather {
    pub station_id: String,
    pub observed: Option<Observed>,
    pub forecasted: Forecasted,
}

/// Observed values over the event window, dated at the window start.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Observed {
    #[serde(with = "time::serde::rfc3339")]
    pub date: OffsetDateTime,
    pub temp_low: i64,
    pub temp_high: i64,
    /// Knots
    pub wind_speed: Option<i64>,
}

/// The forecast baseline for the event window, dated at the window start.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Forecasted {
    #[serde(with = "time::serde::rfc3339")]
    pub date: OffsetDateTime,
    pub temp_low: i64,
    pub temp_high: i64,
    /// Knots
    pub wind_speed: Option<i64>,
}

/// NOAA display rows from stored readings. A station appears once it has
/// both temperature baselines; observations once both temperatures exist.
fn weather_from_readings(
    locations: &[String],
    readings: &[Reading],
    date: OffsetDateTime,
) -> Vec<Weather> {
    let value = |station: &str, metric: &str, observed: bool| {
        readings
            .iter()
            .find(|reading| reading.target == station && reading.metric == metric)
            .and_then(|reading| {
                if observed {
                    reading.observed
                } else {
                    reading.baseline
                }
            })
            .map(|value| value.round() as i64)
    };
    locations
        .iter()
        .filter_map(|station| {
            let forecasted = Forecasted {
                date,
                temp_low: value(station, noaa::TEMP_LOW, false)?,
                temp_high: value(station, noaa::TEMP_HIGH, false)?,
                wind_speed: value(station, noaa::WIND_SPEED, false),
            };
            let observed = match (
                value(station, noaa::TEMP_LOW, true),
                value(station, noaa::TEMP_HIGH, true),
            ) {
                (Some(temp_low), Some(temp_high)) => Some(Observed {
                    date,
                    temp_low,
                    temp_high,
                    wind_speed: value(station, noaa::WIND_SPEED, true),
                }),
                _ => None,
            };
            Some(Weather {
                station_id: station.clone(),
                observed,
                forecasted,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AddEventEntries {
    pub event_id: Uuid,
    pub entries: Vec<AddEventEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AddEventEntry {
    /// Client needs to provide a valid Uuidv7
    pub id: Uuid,
    pub event_id: Uuid,
    /// Predictions, for any source
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub picks: Vec<Pick>,
    /// NOAA-shaped predictions; use instead of `picks` for NOAA events
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_observations: Vec<WeatherChoices>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct WeatherEntry {
    pub id: Uuid,
    pub event_id: Uuid,
    /// The entry's predictions
    pub picks: Vec<Pick>,
    /// The same predictions grouped by NOAA station (NOAA metrics only)
    pub expected_observations: Vec<WeatherChoices>,
    /// Present once the observation window has begun and the oracle scored it
    pub score: Option<i64>,
    pub base_score: Option<i64>,
}

impl From<Entry> for WeatherEntry {
    fn from(entry: Entry) -> Self {
        Self {
            id: entry.id,
            event_id: entry.event_id,
            expected_observations: WeatherChoices::from_picks(&entry.picks),
            picks: entry.picks,
            score: entry.score,
            base_score: entry.base_score,
        }
    }
}

/// Predictions for one NOAA station.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct WeatherChoices {
    /// NOAA station id
    pub stations: String,
    pub temp_high: Option<ValueOptions>,
    pub temp_low: Option<ValueOptions>,
    pub wind_speed: Option<ValueOptions>,
    pub wind_direction: Option<ValueOptions>,
    pub rain_amt: Option<ValueOptions>,
    pub snow_amt: Option<ValueOptions>,
    pub humidity: Option<ValueOptions>,
}

impl WeatherChoices {
    fn slot(&mut self, field: ScoringField) -> &mut Option<ValueOptions> {
        match field {
            ScoringField::TempHigh => &mut self.temp_high,
            ScoringField::TempLow => &mut self.temp_low,
            ScoringField::WindSpeed => &mut self.wind_speed,
            ScoringField::WindDirection => &mut self.wind_direction,
            ScoringField::RainAmt => &mut self.rain_amt,
            ScoringField::SnowAmt => &mut self.snow_amt,
            ScoringField::Humidity => &mut self.humidity,
        }
    }

    /// The predictions made, one per field.
    pub fn predictions(&self) -> impl Iterator<Item = (ScoringField, ValueOptions)> + '_ {
        let mut choices = self.clone();
        ScoringField::ALL
            .into_iter()
            .filter_map(move |field| choices.slot(field).take().map(|choice| (field, choice)))
    }

    /// Groups picks by station, in first-seen order.
    pub fn from_picks(picks: &[Pick]) -> Vec<Self> {
        let mut choices: Vec<Self> = vec![];
        for pick in picks {
            let Some(field) = ScoringField::from_metric(&pick.metric) else {
                continue;
            };
            let index = match choices
                .iter()
                .position(|choice| choice.stations == pick.target)
            {
                Some(index) => index,
                None => {
                    choices.push(Self {
                        stations: pick.target.clone(),
                        ..Self::default()
                    });
                    choices.len() - 1
                }
            };
            *choices[index].slot(field) = Some(pick.prediction);
        }
        choices
    }
}

/// The NOAA metrics [`WeatherChoices`] can name. Ids match [`noaa`]'s.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ScoringField {
    TempHigh,
    TempLow,
    WindSpeed,
    WindDirection,
    RainAmt,
    SnowAmt,
    Humidity,
}

impl ScoringField {
    pub const ALL: [ScoringField; 7] = [
        Self::TempHigh,
        Self::TempLow,
        Self::WindSpeed,
        Self::WindDirection,
        Self::RainAmt,
        Self::SnowAmt,
        Self::Humidity,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TempHigh => noaa::TEMP_HIGH,
            Self::TempLow => noaa::TEMP_LOW,
            Self::WindSpeed => noaa::WIND_SPEED,
            Self::WindDirection => noaa::WIND_DIRECTION,
            Self::RainAmt => noaa::RAIN_AMT,
            Self::SnowAmt => noaa::SNOW_AMT,
            Self::Humidity => noaa::HUMIDITY,
        }
    }

    pub fn from_metric(metric: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.as_str() == metric)
    }
}

impl std::fmt::Display for ScoringField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A prediction relative to the baseline. Serialized as `Over`, `Par`,
/// `Under` on the wire and `over`, `par`, `under` in storage.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub enum ValueOptions {
    Over,
    /// Matches the baseline within the metric's par rule
    Par,
    Under,
}

impl ValueOptions {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Over => "over",
            Self::Par => "par",
            Self::Under => "under",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        [Self::Over, Self::Par, Self::Under]
            .into_iter()
            .find(|option| option.as_str() == value)
    }
}

impl std::fmt::Display for ValueOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{Metric, ObservationWindow, ParRule, SourceId};
    use async_trait::async_trait;
    use std::sync::Arc;

    struct Stations;

    /// A non-weather source: tide gauges with one metric.
    struct Tides;

    #[async_trait]
    impl OutcomeSource for Tides {
        fn id(&self) -> SourceId {
            SourceId::new("tides")
        }
        fn metrics(&self) -> &'static [Metric] {
            &[Metric {
                id: "high_tide_ft",
                par: ParRule::Within(0.1),
            }]
        }
        fn validate_target(&self, _target: &str) -> Result<(), SourceError> {
            Ok(())
        }
        async fn readings(
            &self,
            _window: ObservationWindow,
            _targets: &[String],
        ) -> Result<Vec<Reading>, SourceError> {
            Ok(vec![])
        }
    }

    fn sources() -> Sources {
        Sources::new(
            Arc::new(Stations),
            [Arc::new(Tides) as Arc<dyn OutcomeSource>],
        )
    }

    #[async_trait]
    impl OutcomeSource for Stations {
        fn id(&self) -> SourceId {
            noaa::NOAA_WEATHER
        }
        fn metrics(&self) -> &'static [Metric] {
            &[
                Metric {
                    id: noaa::TEMP_HIGH,
                    par: ParRule::Rounded,
                },
                Metric {
                    id: noaa::TEMP_LOW,
                    par: ParRule::Rounded,
                },
                Metric {
                    id: noaa::WIND_SPEED,
                    par: ParRule::Exact,
                },
            ]
        }
        fn validate_target(&self, target: &str) -> Result<(), SourceError> {
            if target.starts_with('K') {
                Ok(())
            } else {
                Err(SourceError::InvalidTarget {
                    target: target.into(),
                    reason: "not a station".into(),
                })
            }
        }
        async fn readings(
            &self,
            _window: ObservationWindow,
            _targets: &[String],
        ) -> Result<Vec<Reading>, SourceError> {
            Ok(vec![])
        }
    }

    fn key() -> (tempfile::TempDir, SigningKey) {
        let directory = tempfile::tempdir().unwrap();
        let key = SigningKey::load_or_create(&directory.path().join("oracle.pem")).unwrap();
        (directory, key)
    }

    fn event(entries: usize, places: usize) -> CreateEvent {
        let start = OffsetDateTime::now_utc();
        CreateEvent {
            id: Uuid::now_v7(),
            signing_date: start + Duration::hours(3),
            start_observation_date: start,
            end_observation_date: start + Duration::hours(1),
            locations: vec!["KORD".into(), "KSAW".into()],
            number_of_values_per_entry: 3,
            total_allowed_entries: entries,
            number_of_places_win: places,
            source: None,
            scoring_fields: None,
            unlisted: false,
        }
    }

    fn build(event: CreateEvent) -> Result<NewEvent, EventRejection> {
        let (_directory, key) = key();
        NewEvent::build(
            event,
            &sources(),
            &key,
            nostr::key::Keys::generate().public_key(),
        )
    }

    fn record(new: NewEvent) -> EventRecord {
        EventRecord {
            id: new.id,
            source: new.source,
            signing_date: new.signing_date,
            start_observation_date: new.start_observation_date,
            end_observation_date: new.end_observation_date,
            locations: new.locations,
            metrics: new.metrics,
            number_of_values_per_entry: new.number_of_values_per_entry,
            total_allowed_entries: new.total_allowed_entries,
            number_of_places_win: new.number_of_places_win,
            nonce: new.nonce,
            coordinator_pubkey: new.coordinator_pubkey,
            attestation: None,
            total_entries: 0,
            unlisted: new.unlisted,
        }
    }

    #[test]
    fn announcement_commits_to_every_ranking_and_the_refund_outcome() {
        let created = build(event(3, 2)).unwrap();
        // 3 entries, 2 places: 3 * 2 orderings plus one refund-all outcome.
        assert_eq!(created.event_announcement.locking_points.len(), 7);
        assert_eq!(
            i64::from(created.event_announcement.expiry.unwrap()),
            (created.signing_date + Duration::DAY).unix_timestamp()
        );
        assert_eq!(created.signing_date.offset(), UtcOffset::UTC);
    }

    #[test]
    fn invalid_events_are_rejected() {
        let rejected = |change: fn(&mut CreateEvent)| {
            let mut candidate = event(3, 1);
            change(&mut candidate);
            build(candidate).unwrap_err()
        };
        assert!(matches!(
            rejected(|e| e.end_observation_date = e.start_observation_date),
            EventRejection::DatesOutOfOrder
        ));
        assert!(matches!(
            rejected(|e| e.signing_date = e.end_observation_date - Duration::hours(1)),
            EventRejection::DatesOutOfOrder
        ));
        assert!(matches!(
            rejected(|e| e.number_of_places_win = 3),
            EventRejection::Places(3)
        ));
        assert!(matches!(
            rejected(|e| e.number_of_places_win = 0),
            EventRejection::Places(0)
        ));
        assert!(matches!(
            rejected(|e| e.total_allowed_entries = 26),
            EventRejection::Entries(26)
        ));
        assert!(matches!(
            rejected(|e| {
                e.total_allowed_entries = 25;
                e.number_of_places_win = 5;
            }),
            EventRejection::TooManyOutcomes { .. }
        ));
        assert!(matches!(
            rejected(|e| e.locations = vec!["KORD".into(), "KORD".into()]),
            EventRejection::Locations
        ));
        assert!(matches!(
            rejected(|e| e.locations = vec!["bad".into()]),
            EventRejection::InvalidLocation(_)
        ));
        assert!(matches!(
            rejected(|e| e.scoring_fields = Some(vec![])),
            EventRejection::ScoringFields
        ));
        assert!(matches!(
            rejected(|e| e.scoring_fields = Some(vec!["humidity".into()])),
            EventRejection::ScoringFields
        ));
        assert!(matches!(
            rejected(|e| e.number_of_values_per_entry = 7),
            EventRejection::ValuesPerEntry { max: 6, .. }
        ));
        assert!(matches!(
            rejected(|e| e.id = Uuid::from_u128(0x1234)),
            EventRejection::NotUuidV7(_)
        ));
        assert!(matches!(
            rejected(|e| e.source = Some("space_weather".into())),
            EventRejection::UnknownSource(_)
        ));
    }

    #[test]
    fn other_sources_use_the_same_contract_with_picks() {
        let mut tides = event(2, 1);
        tides.source = Some("tides".into());
        tides.locations = vec!["9414290".into()];
        tides.number_of_values_per_entry = 1;
        tides.start_observation_date += Duration::hours(1);
        tides.end_observation_date += Duration::hours(1);
        let created = build(tides).unwrap();
        assert_eq!(created.source, "tides");
        assert_eq!(
            created.metrics,
            vec!["high_tide_ft".to_string()],
            "source defaults"
        );
        let event = record(created);
        let pick = |prediction| AddEventEntry {
            id: Uuid::now_v7(),
            event_id: event.id,
            picks: vec![Pick {
                target: "9414290".into(),
                metric: "high_tide_ft".into(),
                prediction,
            }],
            expected_observations: vec![],
        };
        let now = OffsetDateTime::now_utc();
        let entries = validate_entries(
            &event,
            vec![pick(ValueOptions::Over), pick(ValueOptions::Par)],
            now,
        )
        .unwrap();
        assert_eq!(entries[1].picks[0].prediction, ValueOptions::Par);

        let weather_shaped = AddEventEntry {
            picks: vec![],
            expected_observations: vec![choice("9414290")],
            ..pick(ValueOptions::Par)
        };
        assert!(matches!(
            validate_entries(&event, vec![weather_shaped, pick(ValueOptions::Par)], now),
            Err(EntryRejection::WeatherChoicesUnsupported { .. })
        ));
        let both = AddEventEntry {
            expected_observations: vec![choice("9414290")],
            ..pick(ValueOptions::Par)
        };
        assert!(matches!(
            validate_entries(&event, vec![both, pick(ValueOptions::Par)], now),
            Err(EntryRejection::PickFormat(_))
        ));

        let wire = event.into_event(
            now,
            EventLockingConditions {
                locking_points: vec![],
                expiry: None,
            },
            entries,
            &[Reading {
                target: "9414290".into(),
                metric: "high_tide_ft".into(),
                baseline: Some(5.2),
                observed: None,
            }],
        );
        assert!(wire.weather.is_empty(), "weather rows are NOAA only");
        assert_eq!(wire.readings.len(), 1);
        assert_eq!(wire.entries[0].picks.len(), 1);
        assert!(wire.entries[0].expected_observations.is_empty());
    }

    #[test]
    fn noaa_entries_accept_picks_or_weather_choices() {
        let mut candidate = event(2, 1);
        candidate.start_observation_date += Duration::hours(1);
        candidate.end_observation_date += Duration::hours(1);
        let event = record(build(candidate).unwrap());
        let by_picks = AddEventEntry {
            id: Uuid::now_v7(),
            event_id: event.id,
            picks: vec![Pick {
                target: "KORD".into(),
                metric: noaa::TEMP_HIGH.into(),
                prediction: ValueOptions::Par,
            }],
            expected_observations: vec![],
        };
        let by_choices = entry(&event, vec![choice("KORD")]);
        let entries = validate_entries(
            &event,
            vec![by_picks, by_choices],
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        assert_eq!(entries[0].picks, entries[1].picks);
    }

    fn entry(event: &EventRecord, choices: Vec<WeatherChoices>) -> AddEventEntry {
        AddEventEntry {
            id: Uuid::now_v7(),
            event_id: event.id,
            picks: vec![],
            expected_observations: choices,
        }
    }

    fn choice(station: &str) -> WeatherChoices {
        WeatherChoices {
            stations: station.into(),
            temp_high: Some(ValueOptions::Par),
            ..WeatherChoices::default()
        }
    }

    #[test]
    fn entries_must_fit_the_event() {
        let mut candidate = event(2, 1);
        candidate.start_observation_date += Duration::hours(1);
        candidate.end_observation_date += Duration::hours(1);
        let event = record(build(candidate).unwrap());
        let now = OffsetDateTime::now_utc();
        let valid = || {
            vec![
                entry(&event, vec![choice("KORD")]),
                entry(&event, vec![choice("KSAW")]),
            ]
        };

        let accepted = validate_entries(&event, valid(), now).unwrap();
        assert_eq!(accepted[0].picks[0].metric, noaa::TEMP_HIGH);

        let rejected = |entries| validate_entries(&event, entries, now).unwrap_err();
        assert!(matches!(
            rejected(vec![entry(&event, vec![choice("KORD")])]),
            EntryRejection::Count { .. }
        ));
        assert!(matches!(
            rejected(vec![
                entry(&event, vec![choice("KORD")]),
                entry(&event, vec![choice("KMSP")])
            ]),
            EntryRejection::UnknownLocation { .. }
        ));
        let humidity = WeatherChoices {
            humidity: Some(ValueOptions::Over),
            ..choice("KORD")
        };
        assert!(matches!(
            rejected(vec![
                entry(&event, vec![humidity]),
                entry(&event, vec![choice("KORD")])
            ]),
            EntryRejection::UnscoredField { .. }
        ));
        assert!(matches!(
            rejected(vec![
                entry(&event, vec![choice("KORD"), choice("KORD")]),
                entry(&event, vec![choice("KORD")])
            ]),
            EntryRejection::DuplicatePick { .. }
        ));
        assert!(matches!(
            rejected(vec![
                entry(&event, vec![]),
                entry(&event, vec![choice("KORD")])
            ]),
            EntryRejection::PickCount(..)
        ));
        let first = entry(&event, vec![choice("KORD")]);
        assert!(matches!(
            rejected(vec![first.clone(), first]),
            EntryRejection::DuplicateEntry(_)
        ));
        assert!(matches!(
            validate_entries(&event, valid(), event.end_observation_date).unwrap_err(),
            EntryRejection::Closed
        ));
        let submitted = EventRecord {
            total_entries: 2,
            ..event.clone()
        };
        assert!(matches!(
            validate_entries(&submitted, valid(), now).unwrap_err(),
            EntryRejection::AlreadySubmitted
        ));
    }

    #[test]
    fn choices_round_trip_through_picks() {
        let choices = vec![
            WeatherChoices {
                wind_speed: Some(ValueOptions::Under),
                ..choice("KORD")
            },
            choice("KSAW"),
        ];
        let picks: Vec<Pick> = choices
            .iter()
            .flat_map(|choice| {
                choice.predictions().map(|(field, prediction)| Pick {
                    target: choice.stations.clone(),
                    metric: field.as_str().to_owned(),
                    prediction,
                })
            })
            .collect();
        assert_eq!(picks.len(), 3);
        assert_eq!(WeatherChoices::from_picks(&picks), choices);
    }

    #[test]
    fn events_are_listed_unless_created_unlisted() {
        let mut body = serde_json::to_value(event(3, 1)).unwrap();
        body.as_object_mut().unwrap().remove("unlisted");
        let old_client: CreateEvent = serde_json::from_value(body.clone()).unwrap();
        assert!(!record(build(old_client).unwrap()).unlisted);
        body["unlisted"] = true.into();
        let unlisted: CreateEvent = serde_json::from_value(body).unwrap();
        assert!(record(build(unlisted).unwrap()).unlisted);
    }

    #[test]
    fn status_follows_attestation_then_observation_window() {
        let event = record(build(event(3, 1)).unwrap());
        let start = event.start_observation_date;
        assert_eq!(event.status(start - Duration::SECOND), EventStatus::Live);
        assert_eq!(event.status(start), EventStatus::Running);
        assert_eq!(
            event.status(event.end_observation_date),
            EventStatus::Completed
        );
        let signed = EventRecord {
            attestation: Some(MaybeScalar::Zero),
            ..event
        };
        assert_eq!(signed.status(start), EventStatus::Signed);
    }

    #[test]
    fn stable_encodings() {
        assert_eq!(
            serde_json::to_string(&ScoringField::WindDirection).unwrap(),
            "\"wind_direction\""
        );
        assert_eq!(
            serde_json::to_string(&ValueOptions::Par).unwrap(),
            "\"Par\""
        );
        assert_eq!(
            serde_json::to_string(&EventStatus::Live).unwrap(),
            "\"Live\""
        );
        for field in ScoringField::ALL {
            assert_eq!(ScoringField::from_metric(field.as_str()), Some(field));
            assert_eq!(
                serde_json::to_string(&field).unwrap(),
                format!("\"{}\"", field.as_str())
            );
        }
        for option in [ValueOptions::Over, ValueOptions::Par, ValueOptions::Under] {
            assert_eq!(ValueOptions::from_storage(option.as_str()), Some(option));
        }
    }
}
