//! Event domain types shared by the HTTP API, the database, and the ETL.
//!
//! Wire shapes here are part of the coordinator contract: field names,
//! optionality, and RFC 3339 dates must stay stable across releases.

use anyhow::anyhow;
use dlctix::{
    EventLockingConditions, attestation_locking_point,
    secp::{MaybeScalar, Point, Scalar},
};
use log::info;
use nostr::{key::PublicKey as NostrPublicKey, nips::nip19::ToBech32};
use serde::{Deserialize, Serialize};
use time::{
    Date, Duration, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339,
    macros::format_description,
};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::weather_data::{self, Forecast, Observation};

mod outcomes;

pub use outcomes::{generate_outcome_messages, generate_ranking_permutations};

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateEvent {
    /// Client needs to provide a valid Uuidv7
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    /// Time at which the attestation will be added to the event, needs to be after the end observation date
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations start, all entries must be made before this time, must be before the end observation date
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations ends, must be before the signing date
    pub end_observation_date: OffsetDateTime,
    /// NOAA observation stations used in this event
    pub locations: Vec<String>,
    /// The number of values that can be selected per entry in the event (default to number_of_locations * 3, (temp_low, temp_high, wind_speed))
    pub number_of_values_per_entry: usize,
    /// Total number of allowed entries into the event
    pub total_allowed_entries: usize,
    /// Total number of ranks can win (max 5 ranks)
    pub number_of_places_win: i64,
    /// Which weather fields to use for scoring. Defaults to ["temp_high", "temp_low", "wind_speed"] if not specified.
    /// Available options: temp_high, temp_low, wind_speed, wind_direction, rain_amt, snow_amt, humidity
    #[serde(default = "ScoringField::defaults")]
    pub scoring_fields: Vec<ScoringField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEventData {
    /// Provide UUIDv7 to use for looking up the event
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    /// Time at which the attestation will be added to the event, needs to be after the end observation date
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations start, all entries must be made before this time, must be before the end observation date
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations ends, must be before the signing date
    pub end_observation_date: OffsetDateTime,
    // NOAA observation stations used in this event
    pub locations: Vec<String>,
    /// The number of values that can be selected per entry in the event (default to number_of_locations * 3, (temp_low, temp_high, wind_speed))
    pub number_of_values_per_entry: i64,
    /// Total number of allowed entries into the event
    pub total_allowed_entries: i64,
    /// Total number of ranks can win (max 5 ranks)
    pub number_of_places_win: i64,
    /// Used to sign the result of the event being watched
    pub nonce: Scalar,
    /// Used in constructing the dlctix transactions
    pub event_announcement: EventLockingConditions,
    /// The pubkey of the coordinator
    pub coordinator_pubkey: String,
    /// Which weather fields to use for scoring
    pub scoring_fields: Vec<ScoringField>,
}

impl CreateEventData {
    pub fn new(
        oracle_pubkey: Point,
        coordinator_pubkey: NostrPublicKey,
        event: CreateEvent,
    ) -> Result<Self, anyhow::Error> {
        if event.id.get_version_num() != 7 {
            return Err(anyhow!(
                "Client needs to provide a valid Uuidv7 for event id {}",
                event.id
            ));
        }
        if event.start_observation_date > event.end_observation_date {
            return Err(anyhow!(
                "Start observation date {} needs to be after end observation date {}",
                event.signing_date.format(&Rfc3339)?,
                event.end_observation_date.format(&Rfc3339)?
            ));
        }
        if event.end_observation_date > event.signing_date {
            return Err(anyhow!(
                "Signing date {} needs to be after end observation date {}",
                event.signing_date.format(&Rfc3339)?,
                event.end_observation_date.format(&Rfc3339)?
            ));
        }
        if event.number_of_places_win > 5 {
            return Err(anyhow!(
                "Number of ranks can not be larger than 5, requested {}",
                event.number_of_places_win
            ));
        }
        if event.scoring_fields.is_empty() {
            return Err(anyhow!("At least one scoring field must be selected"));
        }
        let number_of_places_win = usize::try_from(event.number_of_places_win)
            .map_err(|_| anyhow!("Number of ranks must not be negative"))?;
        let possible_user_outcomes: Vec<Vec<usize>> =
            generate_ranking_permutations(event.total_allowed_entries, number_of_places_win);
        info!("user outcomes: {:?}", possible_user_outcomes);

        let outcome_messages: Vec<Vec<u8>> = generate_outcome_messages(possible_user_outcomes);

        let nonce = Scalar::random(&mut rand::rng());
        let nonce_point = nonce.base_point_mul();

        // Manually set expiry to 1 day after the signature should have been provided so users can get their funds back
        let expiry = u32::try_from(
            event
                .signing_date
                .saturating_add(Duration::DAY)
                .unix_timestamp(),
        )
        .map_err(|_| {
            anyhow!(
                "Signing date {} is outside the DLC expiry range",
                event.signing_date
            )
        })?;

        let locking_points = outcome_messages
            .iter()
            .map(|msg| attestation_locking_point(oracle_pubkey, nonce_point, msg))
            .collect();

        // The actual announcement the oracle is going to attest the outcome
        let event_announcement = EventLockingConditions {
            expiry: Some(expiry),
            locking_points,
        };

        let Ok(coordinator_pubkey) = coordinator_pubkey.to_bech32();

        Ok(Self {
            id: event.id,
            start_observation_date: event.start_observation_date.to_offset(UtcOffset::UTC),
            end_observation_date: event.end_observation_date.to_offset(UtcOffset::UTC),
            signing_date: event.signing_date.to_offset(UtcOffset::UTC),
            nonce,
            total_allowed_entries: i64::try_from(event.total_allowed_entries)
                .map_err(|_| anyhow!("Total allowed entries is too large"))?,
            number_of_places_win: event.number_of_places_win,
            number_of_values_per_entry: i64::try_from(event.number_of_values_per_entry)
                .map_err(|_| anyhow!("Number of values per entry is too large"))?,
            locations: event.locations,
            event_announcement,
            coordinator_pubkey,
            scoring_fields: event.scoring_fields,
        })
    }
}

impl From<CreateEventData> for Event {
    fn from(value: CreateEventData) -> Self {
        Self {
            id: value.id,
            signing_date: value.signing_date,
            start_observation_date: value.start_observation_date,
            end_observation_date: value.end_observation_date,
            locations: value.locations,
            total_allowed_entries: value.total_allowed_entries,
            number_of_places_win: value.number_of_places_win,
            number_of_values_per_entry: value.number_of_values_per_entry,
            event_announcement: value.event_announcement,
            nonce: value.nonce,
            status: EventStatus::default(),
            entry_ids: vec![],
            entries: vec![],
            weather: vec![],
            attestation: None,
            coordinator_pubkey: value.coordinator_pubkey,
            scoring_fields: value.scoring_fields,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
pub struct EventFilter {
    // TODO: add more options, proper pagination and search
    pub limit: Option<usize>,
    pub event_ids: Option<Vec<Uuid>>,
}

impl Default for EventFilter {
    fn default() -> Self {
        Self {
            limit: Some(100_usize),
            event_ids: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SignEvent {
    pub id: Uuid,
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end_observation_date: OffsetDateTime,
    pub status: EventStatus,
    #[schema(value_type = String)]
    pub nonce: Scalar,
    #[schema(value_type = String)]
    pub event_announcement: EventLockingConditions,
    pub number_of_places_win: i64,
    pub number_of_values_per_entry: i64,
    #[schema(value_type = String)]
    pub attestation: Option<MaybeScalar>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ActiveEvent {
    pub id: Uuid,
    pub locations: Vec<String>,
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end_observation_date: OffsetDateTime,
    pub status: EventStatus,
    pub total_allowed_entries: i64,
    pub total_entries: i64,
    pub number_of_values_per_entry: i64,
    pub number_of_places_win: i64,
    #[schema(value_type = String)]
    pub attestation: Option<MaybeScalar>,
    /// Which weather fields are used for scoring in this event
    pub scoring_fields: Vec<ScoringField>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub enum EventStatus {
    /// Observation date has not passed yet and entries can be added
    #[default]
    Live,
    /// Currently in the Observation date, entries cannot be added
    Running,
    /// Event Observation window has finished, not yet signed
    Completed,
    /// Event has completed and been signed by the oracle
    Signed,
}

impl std::fmt::Display for EventStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Live => write!(f, "live"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Signed => write!(f, "signed"),
        }
    }
}

impl TryFrom<&str> for EventStatus {
    type Error = anyhow::Error;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "live" => Ok(EventStatus::Live),
            "running" => Ok(EventStatus::Running),
            "completed" => Ok(EventStatus::Completed),
            "signed" => Ok(EventStatus::Signed),
            val => Err(anyhow!("invalid status: {}", val)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct EventSummary {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    /// Time at which the attestation will be added to the event, needs to be after the end observation date
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations start, all entries must be made before this time, must be before the end observation date
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations ends, must be before the signing date
    pub end_observation_date: OffsetDateTime,
    /// NOAA observation stations used in this event
    pub locations: Vec<String>,
    /// The number of values that can be selected per entry in the event (default to number_of_locations * 3, (temp_low, temp_high, wind_speed))
    pub number_of_values_per_entry: i64,
    /// Current status of the event, where in the lifecyle are we (LIVE, RUNNING, COMPLETED, SIGNED, defaults to LIVE)
    pub status: EventStatus,
    /// Knowing the total number of entries, how many can place
    /// The dlctix coordinator can determine how many transactions to create
    pub total_allowed_entries: i64,
    /// Needs to all be generated at the start
    pub total_entries: i64,
    pub number_of_places_win: i64,
    /// The forecasted and observed values for each station on the event date
    pub weather: Vec<Weather>,
    /// When added it means the oracle has signed that the current data is the final result
    #[schema(value_type = String)]
    pub attestation: Option<MaybeScalar>,
    /// Used to sign the result of the event being watched
    #[schema(value_type = String)]
    pub nonce: Scalar,
}

pub fn get_status(
    attestation: Option<MaybeScalar>,
    start_observation_date: OffsetDateTime,
    end_observation_date: OffsetDateTime,
) -> EventStatus {
    if attestation.is_some() {
        return EventStatus::Signed;
    }

    let now = OffsetDateTime::now_utc();

    if now < start_observation_date {
        return EventStatus::Live;
    }

    if now < end_observation_date {
        return EventStatus::Running;
    }

    EventStatus::Completed
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Event {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    /// Time at which the attestation will be added to the event, needs to be after the end observation date
    pub signing_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations start, all entries must be made before this time, must be before the end observation date
    pub start_observation_date: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    /// Time when the weather observations ends, must be before the signing date
    pub end_observation_date: OffsetDateTime,
    /// NOAA observation stations used in this event
    pub locations: Vec<String>,
    /// The number of values that can be selected per entry in the event (default to number_of_locations * 3, (temp_low, temp_high, wind_speed))
    pub number_of_values_per_entry: i64,
    /// Current status of the event, where in the lifecyle are we (LIVE, RUNNING, COMPLETED, SIGNED)
    pub status: EventStatus,
    /// Knowing the total number of entries, how many can place
    /// The dlctix coordinator can determine how many transactions to create
    pub total_allowed_entries: i64,
    /// Needs to all be generated at the start
    pub entry_ids: Vec<Uuid>,
    pub number_of_places_win: i64,
    /// All entries into this event, wont be returned until date of observation begins and will be ranked by score
    pub entries: Vec<WeatherEntry>,
    /// The forecasted and observed values for each station on the event date
    pub weather: Vec<Weather>,
    /// Nonce the oracle committed to use as part of signing final results
    #[schema(value_type = String)]
    pub nonce: Scalar,
    /// Holds the predefined outcomes the oracle will attest to at event complete
    #[schema(value_type = String)]
    pub event_announcement: EventLockingConditions,
    /// When added it means the oracle has signed that the current data is the final result
    #[schema(value_type = String)]
    pub attestation: Option<MaybeScalar>,
    /// The pubkey of the coordinator
    pub coordinator_pubkey: String,
    /// Which weather fields are used for scoring in this event
    pub scoring_fields: Vec<ScoringField>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Weather {
    pub station_id: String,
    pub observed: Option<Observed>,
    pub forecasted: Forecasted,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Observed {
    #[serde(with = "time::serde::rfc3339")]
    pub date: OffsetDateTime,
    pub temp_low: i64,
    pub temp_high: i64,
    pub wind_speed: i64,
}

impl TryFrom<&Observation> for Observed {
    type Error = weather_data::Error;

    fn try_from(value: &Observation) -> Result<Observed, Self::Error> {
        Ok(Self {
            date: OffsetDateTime::parse(&value.start_time, &Rfc3339)?,
            temp_low: value.temp_low.round() as i64,
            temp_high: value.temp_high.round() as i64,
            wind_speed: value.wind_speed,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Forecasted {
    #[serde(with = "time::serde::rfc3339")]
    pub date: OffsetDateTime,
    pub temp_low: i64,
    pub temp_high: i64,
    pub wind_speed: Option<i64>,
}

impl TryFrom<&Forecast> for Forecasted {
    type Error = weather_data::Error;

    fn try_from(value: &Forecast) -> Result<Forecasted, Self::Error> {
        let format = format_description!("[year]-[month]-[day]");
        let date = Date::parse(&value.date, format)?;
        Ok(Self {
            date: date.midnight().assume_utc(),
            temp_low: value.temp_low,
            temp_high: value.temp_high,
            wind_speed: value.wind_speed,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AddEventEntries {
    pub event_id: Uuid,
    pub entries: Vec<AddEventEntry>,
}

// Once submitted for now don't allow changes
// Decide if we want to add a pubkey for who submitted the entry?
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AddEventEntry {
    /// Client needs to provide a valid Uuidv7
    pub id: Uuid,
    pub event_id: Uuid,
    pub expected_observations: Vec<WeatherChoices>,
}

impl From<AddEventEntry> for WeatherEntry {
    fn from(value: AddEventEntry) -> Self {
        WeatherEntry {
            id: value.id,
            event_id: value.event_id,
            expected_observations: value.expected_observations,
            score: None,
            base_score: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct WeatherEntry {
    pub id: Uuid,
    pub event_id: Uuid,
    pub expected_observations: Vec<WeatherChoices>,
    /// A score wont appear until the observation_date has begun
    pub score: Option<i64>,
    pub base_score: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct WeatherChoices {
    // NOAA weather stations we're using
    pub stations: String,
    pub temp_high: Option<ValueOptions>,
    pub temp_low: Option<ValueOptions>,
    pub wind_speed: Option<ValueOptions>,
    pub wind_direction: Option<ValueOptions>,
    pub rain_amt: Option<ValueOptions>,
    pub snow_amt: Option<ValueOptions>,
    pub humidity: Option<ValueOptions>,
}

/// Available fields that can be used for scoring in an event
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq, Hash)]
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

impl std::fmt::Display for ScoringField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TempHigh => write!(f, "temp_high"),
            Self::TempLow => write!(f, "temp_low"),
            Self::WindSpeed => write!(f, "wind_speed"),
            Self::WindDirection => write!(f, "wind_direction"),
            Self::RainAmt => write!(f, "rain_amt"),
            Self::SnowAmt => write!(f, "snow_amt"),
            Self::Humidity => write!(f, "humidity"),
        }
    }
}

impl TryFrom<&str> for ScoringField {
    type Error = anyhow::Error;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "temp_high" => Ok(ScoringField::TempHigh),
            "temp_low" => Ok(ScoringField::TempLow),
            "wind_speed" => Ok(ScoringField::WindSpeed),
            "wind_direction" => Ok(ScoringField::WindDirection),
            "rain_amt" => Ok(ScoringField::RainAmt),
            "snow_amt" => Ok(ScoringField::SnowAmt),
            "humidity" => Ok(ScoringField::Humidity),
            val => Err(anyhow!("invalid scoring field: {}", val)),
        }
    }
}

impl ScoringField {
    /// Returns the default scoring fields (original behavior)
    pub fn defaults() -> Vec<ScoringField> {
        vec![
            ScoringField::TempHigh,
            ScoringField::TempLow,
            ScoringField::WindSpeed,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub enum ValueOptions {
    Over,
    // Par is what was forecasted for this value
    Par,
    Under,
}

impl std::fmt::Display for ValueOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Over => write!(f, "over"),
            Self::Par => write!(f, "par"),
            Self::Under => write!(f, "under"),
        }
    }
}

impl TryFrom<&str> for ValueOptions {
    type Error = anyhow::Error;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "over" => Ok(ValueOptions::Over),
            "par" => Ok(ValueOptions::Par),
            "under" => Ok(ValueOptions::Under),
            val => Err(anyhow!("invalid option: {}", val)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(entries: usize, places: i64) -> CreateEvent {
        let start = OffsetDateTime::now_utc();
        CreateEvent {
            id: Uuid::now_v7(),
            signing_date: start + Duration::hours(3),
            start_observation_date: start,
            end_observation_date: start + Duration::hours(1),
            locations: vec!["KORD".into()],
            number_of_values_per_entry: 3,
            total_allowed_entries: entries,
            number_of_places_win: places,
            scoring_fields: ScoringField::defaults(),
        }
    }

    fn oracle_point() -> Point {
        Scalar::random(&mut rand::rng()).base_point_mul()
    }

    #[test]
    fn announcement_commits_to_every_ranking_and_the_refund_outcome() {
        let created = CreateEventData::new(
            oracle_point(),
            nostr::key::Keys::generate().public_key(),
            event(3, 2),
        )
        .unwrap();
        // 3 entries, 2 places: 3 * 2 orderings plus one refund-all outcome.
        assert_eq!(created.event_announcement.locking_points.len(), 7);
        let expiry = created.event_announcement.expiry.unwrap();
        assert_eq!(
            i64::from(expiry),
            (created.signing_date + Duration::DAY).unix_timestamp()
        );
        assert_eq!(created.signing_date.offset(), UtcOffset::UTC);
    }

    #[test]
    fn invalid_dates_and_ranks_are_rejected() {
        let pubkey = nostr::key::Keys::generate().public_key();
        let mut reversed = event(3, 1);
        reversed.end_observation_date = reversed.start_observation_date - Duration::hours(1);
        assert!(CreateEventData::new(oracle_point(), pubkey, reversed).is_err());
        let mut late_signing = event(3, 1);
        late_signing.signing_date = late_signing.end_observation_date - Duration::hours(1);
        assert!(CreateEventData::new(oracle_point(), pubkey, late_signing).is_err());
        assert!(CreateEventData::new(oracle_point(), pubkey, event(3, 6)).is_err());
        let mut no_fields = event(3, 1);
        no_fields.scoring_fields.clear();
        assert!(CreateEventData::new(oracle_point(), pubkey, no_fields).is_err());
        let mut v4 = event(3, 1);
        v4.id = Uuid::from_u128(0x1234);
        assert!(CreateEventData::new(oracle_point(), pubkey, v4).is_err());
    }

    #[test]
    fn status_follows_attestation_then_observation_window() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            get_status(None, now + Duration::hours(1), now + Duration::hours(2)),
            EventStatus::Live
        );
        assert_eq!(
            get_status(None, now - Duration::hours(1), now + Duration::hours(1)),
            EventStatus::Running
        );
        assert_eq!(
            get_status(None, now - Duration::hours(2), now - Duration::hours(1)),
            EventStatus::Completed
        );
        assert_eq!(
            get_status(
                Some(MaybeScalar::Zero),
                now + Duration::hours(1),
                now + Duration::hours(2)
            ),
            EventStatus::Signed
        );
    }
}
