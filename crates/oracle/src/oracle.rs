//! Oracle behaviour: event creation, entry validation, scoring, and
//! attestation. All persistence goes through [`Database`]; all weather data
//! comes from a [`WeatherData`] implementation.

use crate::{
    database::{Database, WriteError},
    events::{
        ActiveEvent, AddEventEntry, CreateEvent, CreateEventData, Event, EventFilter, EventStatus,
        EventSummary, Forecasted, Observed, ScoringField, SignEvent, ValueOptions, Weather,
        WeatherEntry,
    },
    routes::{ForecastRequest, ObservationRequest, TemperatureUnit},
    weather_data::{self, Forecast, Observation, WeatherData, validate_station_id},
};
use anyhow::anyhow;
use base64::{Engine, engine::general_purpose};
use dlctix::{
    attestation_locking_point, attestation_secret,
    musig2::secp256k1::{PublicKey, Secp256k1, SecretKey},
    secp::{MaybePoint, Point},
};
use log::{debug, error, info, warn};
use nostr::{
    key::{Keys, PublicKey as NostrPublicKey},
    nips::nip19::ToBech32,
};
use pem_rfc7468::{decode_vec, encode_string};
use serde::Serialize;
use std::{
    cmp,
    fs::{File, metadata},
    io::{Read, Write},
    path::Path,
    sync::Arc,
};
use thiserror::Error;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

/// The oracle refuses events it cannot enumerate outcomes for in reasonable
/// time; see `generate_ranking_permutations`.
const MAX_ALLOWED_ENTRIES: usize = 25;
const MAX_PLACES_WIN: i64 = 5;

#[derive(Error, Debug, Serialize, ToSchema)]
pub enum Error {
    #[error("{0}")]
    NotFound(String),
    #[schema(value_type = String)]
    #[error("Failed to load oracle key: {0}")]
    Key(#[serde(skip)] anyhow::Error),
    #[schema(value_type = String)]
    #[error("Failed to convert private key into nostr keys: {0}")]
    ConvertKey(
        #[serde(skip)]
        #[from]
        nostr::error::Error,
    ),
    #[schema(value_type = String)]
    #[error("Failed to query datasource: {0}")]
    DataQuery(#[serde(skip)] Box<sqlx::Error>),
    #[schema(value_type = String)]
    #[error("{0}")]
    Write(
        #[serde(skip)]
        #[from]
        WriteError,
    ),
    #[error("Pubkeys in DB doesn't match with .pem")]
    MismatchPubkey(String),
    #[error("Invalid entry: {0}")]
    BadEntry(String),
    #[error("Invalid event: {0}")]
    #[schema(value_type = String)]
    BadEvent(#[serde(skip)] anyhow::Error),
    #[schema(value_type = String)]
    #[error("{0}")]
    WeatherData(
        #[serde(skip)]
        #[from]
        weather_data::Error,
    ),
    #[error("Failed to find winning outcome: {0}")]
    OutcomeNotFound(String),
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Error::DataQuery(Box::new(error))
    }
}

pub struct Oracle {
    db: Database,
    weather_data: Arc<dyn WeatherData>,
    private_key: SecretKey,
    public_key: PublicKey,
}

impl Oracle {
    /// Loads (or creates) the signing key and checks it matches the key the
    /// database was created with.
    pub async fn new(
        db: Database,
        weather_data: Arc<dyn WeatherData>,
        private_key_file_path: &Path,
    ) -> Result<Self, Error> {
        let secret_key = get_key(private_key_file_path).map_err(Error::Key)?;
        let secp = Secp256k1::new();
        let public_key = secret_key.public_key(&secp);
        let oracle = Self {
            db,
            weather_data,
            private_key: secret_key,
            public_key,
        };
        oracle.validate_oracle_metadata().await?;
        Ok(oracle)
    }

    async fn validate_oracle_metadata(&self) -> Result<(), Error> {
        let own_key = self.public_key.x_only_public_key().0;
        match self.db.get_stored_public_key().await? {
            Some(stored_public_key) if stored_public_key != own_key => {
                Err(Error::MismatchPubkey(format!(
                    "stored_pubkey: {:?} pem_pubkey: {:?}",
                    stored_public_key,
                    self.public_key()
                )))
            }
            Some(_) => Ok(()),
            None => Ok(self.db.add_oracle_metadata(own_key).await?),
        }
    }

    pub fn raw_public_key(&self) -> PublicKey {
        self.public_key
    }

    pub fn public_key(&self) -> String {
        let key = Point::from(self.public_key).serialize();
        general_purpose::STANDARD.encode(key)
    }

    pub fn npub(&self) -> Result<String, Error> {
        let secret_key = self.private_key.display_secret().to_string();
        let keys = Keys::parse(&secret_key)?;
        let Ok(npub) = keys.public_key().to_bech32();
        Ok(npub)
    }

    pub async fn list_events(&self, filter: EventFilter) -> Result<Vec<EventSummary>, Error> {
        Ok(self.db.filtered_list_events(filter).await?)
    }

    pub async fn get_event(&self, id: &Uuid) -> Result<Event, Error> {
        self.db
            .get_event(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("event with id {} not found", id)))
    }

    pub async fn create_event(
        &self,
        coordinator_pubkey: NostrPublicKey,
        event: CreateEvent,
    ) -> Result<Event, Error> {
        if event.id.get_version_num() != 7 {
            return Err(Error::BadEvent(anyhow!(
                "event needs to provide a valid Uuidv7 for event id {}",
                event.id
            )));
        }
        if event.total_allowed_entries > MAX_ALLOWED_ENTRIES {
            return Err(Error::BadEvent(anyhow!(
                "Max number of allowed entries the oracle can watch is {MAX_ALLOWED_ENTRIES}"
            )));
        }
        if event.number_of_places_win > MAX_PLACES_WIN {
            return Err(Error::BadEvent(anyhow!(
                "Max number of allowed ranks in an event that can win is {MAX_PLACES_WIN}, requested: {}",
                event.number_of_places_win
            )));
        }
        if event.locations.is_empty() {
            return Err(Error::BadEvent(anyhow!(
                "event needs at least one station location"
            )));
        }
        for location in &event.locations {
            validate_station_id(location).map_err(|error| Error::BadEvent(anyhow!("{error}")))?;
        }

        let oracle_event = CreateEventData::new(
            Point::from(self.raw_public_key()),
            coordinator_pubkey,
            event,
        )
        .map_err(Error::BadEvent)?;
        Ok(self.db.add_event(oracle_event).await?)
    }

    pub async fn add_event_entries(
        &self,
        nostr_pubkey: NostrPublicKey,
        event_id: Uuid,
        entries: Vec<AddEventEntry>,
    ) -> Result<Vec<WeatherEntry>, Error> {
        let Ok(nostr_pubkey) = nostr_pubkey.to_bech32();
        if event_id.get_version_num() != 7 {
            return Err(Error::BadEntry(format!(
                "Client needs to provide a valid Uuidv7 for event id {}",
                event_id
            )));
        }
        let event = self.get_event(&event_id).await?;
        if !event.entries.is_empty() {
            return Err(Error::BadEntry(format!(
                "event {} already has entries, no more entries are allowed",
                event.id
            )));
        }
        if usize::try_from(event.total_allowed_entries).ok() != Some(entries.len()) {
            return Err(Error::BadEntry(format!(
                "Client needs to provide {} entries for this event {}",
                event.total_allowed_entries, event_id
            )));
        }
        if event.coordinator_pubkey != nostr_pubkey {
            return Err(Error::BadEntry(format!(
                "Client needs to the valid coordinator signature in header for this event {}",
                event_id
            )));
        }
        let mut weather_entries: Vec<WeatherEntry> = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.event_id != event_id {
                return Err(Error::BadEntry(format!(
                    "Client add entries to be for this event {}, entry {} has the wrong event id {}",
                    event_id, entry.id, entry.event_id
                )));
            }
            validate_event_entry(&entry, &event)?;
            weather_entries.push(entry.into());
        }
        self.db.add_event_entries(weather_entries.clone()).await?;

        Ok(weather_entries)
    }

    pub async fn get_running_events(&self) -> Result<Vec<ActiveEvent>, Error> {
        Ok(self.db.get_active_events().await?)
    }

    pub async fn get_event_entry(
        &self,
        event_id: &Uuid,
        entry_id: &Uuid,
    ) -> Result<WeatherEntry, Error> {
        self.db
            .get_weather_entry(event_id, entry_id)
            .await?
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "entry with id {} not found for event {}",
                    entry_id, event_id
                ))
            })
    }

    /// Refreshes weather readings, scores entries, and signs completed
    /// events. Runs after every data upload; safe to run repeatedly.
    pub async fn etl_data(&self, etl_process_id: u64) -> Result<(), Error> {
        // NOTE: Making the assumption the number of active events will remain small, maybe 10 at most for now,
        // Also assuming it's okay to have duplicate location weather reading rows for now (if this becomes a problem we will need to de-dup)
        info!(" etl_process_id {}, starting etl process", etl_process_id);
        let events_to_update = self.get_running_events().await?;
        // 1) update weather readings
        self.update_event_weather_data(etl_process_id, &events_to_update)
            .await?;
        // 2) update entry scores for running & completed events
        let events: Vec<ActiveEvent> = events_to_update
            .iter()
            .filter(|entry| {
                (entry.status == EventStatus::Running || entry.status == EventStatus::Completed)
                    && entry.attestation.is_none()
            })
            .cloned()
            .collect();
        self.update_active_events_entry_scores(etl_process_id, events)
            .await?;
        // 3) sign results for events that are completed and need it
        let events_to_sign: Vec<Uuid> = events_to_update
            .iter()
            .filter(|event| event.status == EventStatus::Completed && event.attestation.is_none())
            .map(|event| event.id)
            .collect();
        if events_to_sign.is_empty() {
            info!(
                " etl_process_id {}, no events to sign, completed etl process",
                etl_process_id
            );
            return Ok(());
        }
        self.add_oracle_signature(etl_process_id, &events_to_sign)
            .await?;
        info!(" etl_process_id {}, completed etl process", etl_process_id);
        Ok(())
    }

    async fn update_event_weather_data(
        &self,
        etl_process_id: u64,
        events_to_update: &[ActiveEvent],
    ) -> Result<(), Error> {
        for event in events_to_update {
            info!(
                "updating event {} with status {} weather data in process {}",
                event.id, event.status, etl_process_id
            );
            let forecast_data = self.event_forecast_data(event).await?;
            let observation_data = if event.start_observation_date > OffsetDateTime::now_utc() {
                vec![]
            } else {
                self.event_observation_data(event).await?
            };
            let weather = merge_weather(event, &forecast_data, &observation_data)?;
            self.db
                .update_weather_station_data(event.id, weather)
                .await?;
            info!(
                "completed event {} weather data update {} in process {}",
                event.id, event.status, etl_process_id
            );
        }
        info!(
            "completed updating all event weather data in etl process {}",
            etl_process_id
        );
        Ok(())
    }

    async fn update_active_events_entry_scores(
        &self,
        etl_process_id: u64,
        events: Vec<ActiveEvent>,
    ) -> Result<(), Error> {
        info!(
            "starting to update all event entry scores in etl process {}",
            etl_process_id
        );
        for event in events {
            self.update_entry_scores(etl_process_id, event).await?;
        }
        info!(
            "completed updating all event entry scores in etl process {}",
            etl_process_id
        );
        Ok(())
    }

    async fn update_entry_scores(
        &self,
        etl_process_id: u64,
        event: ActiveEvent,
    ) -> Result<(), Error> {
        let entries: Vec<WeatherEntry> = self.db.get_event_weather_entries(&event.id).await?;

        let observation_data = self.event_observation_data(&event).await?;
        let forecast_data = self.event_forecast_data(&event).await?;
        let mut entry_scores: Vec<(Uuid, i64, i64)> = vec![];

        for entry in entries {
            if entry.event_id != event.id {
                warn!("entry {} not in this event {}", entry.id, event.id);
                continue;
            }
            let base_score = score_entry(&event, &entry, &forecast_data, &observation_data);
            let total_score = total_score(&entry, base_score)?;
            info!(
                "updating entry {} for event {} to score {} in etl process {}",
                entry.id, event.id, total_score, etl_process_id
            );
            let base_score = i64::try_from(base_score).unwrap_or(i64::MAX);
            entry_scores.push((entry.id, total_score, base_score));
        }

        self.db.update_entry_scores(entry_scores).await?;

        Ok(())
    }

    async fn add_oracle_signature(
        &self,
        etl_process_id: u64,
        event_ids: &[Uuid],
    ) -> Result<(), Error> {
        let events: Vec<SignEvent> = self.db.get_events_to_sign(event_ids).await?;
        debug!("events to sign: {:?}", events);
        for event in &events {
            if event.signing_date >= OffsetDateTime::now_utc() {
                continue;
            }
            let entries = self.db.get_event_weather_entries(&event.id).await?;
            let winners = winning_indices(&entries, event.number_of_places_win);
            let nonce_point = event.nonce.base_point_mul();
            let winner_bytes = get_winning_bytes(&winners);
            let locking_point =
                attestation_locking_point(self.public_key, nonce_point, &winner_bytes);
            info!("winner_bytes: {:?}", winner_bytes);

            let mut entry_indices = entries.clone();
            // very important, the sort index of the entry should always be the same when getting the outcome
            entry_indices.sort_by_key(|entry| entry.id);
            let winners_str = winners
                .iter()
                .filter_map(|entry_index| entry_indices.get(*entry_index))
                .map(|entry| format!("({}, {})", entry.score.unwrap_or_default(), entry.id))
                .collect::<Vec<String>>()
                .join(", ");

            let MaybePoint::Valid(_) = locking_point else {
                // Something went horribly wrong, use the info from this log line to track refunding users based on DLC expiry
                error!(
                    "final result doesn't match any of the possible outcomes: event_id {} winners {} expiry {:?}",
                    event.id, winners_str, event.event_announcement.expiry
                );
                return Err(Error::OutcomeNotFound(format!(
                    "event_id {} outcome winners {} expiry {:?}",
                    event.id, winners_str, event.event_announcement.expiry
                )));
            };

            info!("winners: event_id {} winners {}", event.id, winners_str);

            let attestation = attestation_secret(self.private_key, event.nonce, &winner_bytes);
            self.db
                .update_event_attestation(event.id, attestation)
                .await?;
        }
        info!(
            "completed adding oracle signature to all events that need it in etl process {}",
            etl_process_id
        );
        Ok(())
    }

    async fn event_forecast_data(&self, event: &ActiveEvent) -> Result<Vec<Forecast>, Error> {
        // Locations were validated when the event was created
        let forecast_requests = ForecastRequest {
            start: Some(event.start_observation_date),
            end: Some(event.end_observation_date),
            generated_start: None,
            generated_end: None,
            station_ids: event.locations.join(","),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        Ok(self
            .weather_data
            .forecasts_data(&forecast_requests, event.locations.clone())
            .await?)
    }

    async fn event_observation_data(&self, event: &ActiveEvent) -> Result<Vec<Observation>, Error> {
        let observation_requests = ObservationRequest {
            start: Some(event.start_observation_date),
            end: Some(event.end_observation_date),
            station_ids: event.locations.join(","),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        Ok(self
            .weather_data
            .observation_data(&observation_requests, event.locations.clone())
            .await?)
    }
}

fn validate_event_entry(entry: &AddEventEntry, event: &Event) -> Result<(), Error> {
    if entry.id.get_version_num() != 7 {
        return Err(Error::BadEntry(format!(
            "Client needs to provide a valid Uuidv7 for entry id {}",
            entry.id
        )));
    }

    let mut choice_count = 0;
    for weather_choice in &entry.expected_observations {
        choice_count += [
            weather_choice.temp_high.is_some(),
            weather_choice.temp_low.is_some(),
            weather_choice.wind_speed.is_some(),
            weather_choice.wind_direction.is_some(),
            weather_choice.rain_amt.is_some(),
            weather_choice.snow_amt.is_some(),
            weather_choice.humidity.is_some(),
        ]
        .into_iter()
        .filter(|chosen| *chosen)
        .count();

        if i64::try_from(choice_count).unwrap_or(i64::MAX) > event.number_of_values_per_entry {
            return Err(Error::BadEntry(format!(
                "entry_id {0} not valid, too many value choices, max allowed {1} but got {2}",
                entry.id, event.number_of_values_per_entry, choice_count
            )));
        }
    }

    let all_valid_locations = entry
        .expected_observations
        .iter()
        .all(|choice| event.locations.contains(&choice.stations));
    if !all_valid_locations {
        return Err(Error::BadEntry(format!(
            "entry_id {0} not valid, choose locations not in the event",
            entry.id
        )));
    }
    Ok(())
}

const OVER_OR_UNDER_POINTS: u64 = 10;
const PAR_POINTS: u64 = 20;

fn points<T: PartialOrd>(choice: &ValueOptions, forecast: T, observed: T) -> u64 {
    match choice {
        ValueOptions::Over if observed > forecast => OVER_OR_UNDER_POINTS,
        ValueOptions::Par if observed == forecast => PAR_POINTS,
        ValueOptions::Under if observed < forecast => OVER_OR_UNDER_POINTS,
        _ => 0,
    }
}

fn points_within<T>(choice: &ValueOptions, forecast: T, observed: T, tolerance: T) -> u64
where
    T: PartialOrd + Copy + std::ops::Sub<Output = T> + Signed,
{
    match choice {
        ValueOptions::Over if observed > forecast => OVER_OR_UNDER_POINTS,
        ValueOptions::Par if (observed - forecast).absolute() <= tolerance => PAR_POINTS,
        ValueOptions::Under if observed < forecast => OVER_OR_UNDER_POINTS,
        _ => 0,
    }
}

trait Signed {
    fn absolute(self) -> Self;
}

impl Signed for i64 {
    fn absolute(self) -> Self {
        self.abs()
    }
}

impl Signed for f64 {
    fn absolute(self) -> Self {
        self.abs()
    }
}

/// Base score: par earns 20 points, a correct over/under earns 10, per
/// enabled scoring field and station.
fn score_entry(
    event: &ActiveEvent,
    entry: &WeatherEntry,
    forecast_data: &[Forecast],
    observation_data: &[Observation],
) -> u64 {
    let scoring_fields = &event.scoring_fields;
    let mut base_score = 0;
    for location in &event.locations {
        let Some(choice) = entry
            .expected_observations
            .iter()
            .find(|expected| expected.stations == *location)
        else {
            continue;
        };
        let Some(forecast) = forecast_data
            .iter()
            .find(|forecast| forecast.station_id == *location)
        else {
            warn!("no forecast found for: {}", location);
            continue;
        };
        let Some(observation) = observation_data
            .iter()
            .find(|observation| observation.station_id == *location)
        else {
            warn!("no observation found for: {}", location);
            continue;
        };

        if scoring_fields.contains(&ScoringField::TempHigh)
            && let Some(choice) = &choice.temp_high
        {
            base_score += points(
                choice,
                forecast.temp_high,
                observation.temp_high.round() as i64,
            );
        }
        if scoring_fields.contains(&ScoringField::TempLow)
            && let Some(choice) = &choice.temp_low
        {
            base_score += points(
                choice,
                forecast.temp_low,
                observation.temp_low.round() as i64,
            );
        }
        if scoring_fields.contains(&ScoringField::WindSpeed)
            && let Some(choice) = &choice.wind_speed
        {
            // A missing NOAA wind forecast is an implicit calm (0) prediction
            base_score += points(
                choice,
                forecast.wind_speed.unwrap_or(0),
                observation.wind_speed,
            );
        }
        if scoring_fields.contains(&ScoringField::WindDirection)
            && let Some(choice) = &choice.wind_direction
        {
            let forecast_dir = forecast.wind_direction.unwrap_or(0);
            let observed_dir = observation.wind_direction.unwrap_or(0);
            // Par when within 22 degrees around the compass
            let diff = ((forecast_dir - observed_dir).abs() % 360)
                .min(360 - ((forecast_dir - observed_dir).abs() % 360));
            base_score += match choice {
                ValueOptions::Over if observed_dir > forecast_dir => OVER_OR_UNDER_POINTS,
                ValueOptions::Par if diff <= 22 => PAR_POINTS,
                ValueOptions::Under if observed_dir < forecast_dir => OVER_OR_UNDER_POINTS,
                _ => 0,
            };
        }
        if scoring_fields.contains(&ScoringField::RainAmt)
            && let Some(choice) = &choice.rain_amt
        {
            // Par for rain: within 0.1 inches
            base_score += points_within(
                choice,
                forecast.rain_amt.unwrap_or(0.0),
                observation.rain_amt.unwrap_or(0.0),
                0.1,
            );
        }
        if scoring_fields.contains(&ScoringField::SnowAmt)
            && let Some(choice) = &choice.snow_amt
        {
            // Par for snow: within 0.5 inches
            base_score += points_within(
                choice,
                forecast.snow_amt.unwrap_or(0.0),
                observation.snow_amt.unwrap_or(0.0),
                0.5,
            );
        }
        if scoring_fields.contains(&ScoringField::Humidity)
            && let Some(choice) = &choice.humidity
        {
            // Par for humidity: within 5%, compared against the forecast maximum
            base_score += points_within(
                choice,
                forecast.humidity_max.unwrap_or(0),
                observation.humidity.unwrap_or(0),
                5,
            );
        }
    }
    base_score
}

/// Total score: `max(1, base) * 10_000 - (entry creation millis % 10_000)`.
///
/// Higher base scores dominate; for equal scores the earlier UUIDv7 entry
/// ranks higher. Four timestamp digits keep the outcome space small while
/// making collisions negligible for up to 10,000 entries a day.
fn total_score(entry: &WeatherEntry, base_score: u64) -> Result<i64, Error> {
    let (created_at_secs, created_at_nano) = entry
        .id
        .get_timestamp()
        .ok_or_else(|| Error::BadEntry(format!("entry {} is not a UUIDv7", entry.id)))?
        .to_unix();
    let time_millis = (created_at_secs * 1000) + (u64::from(created_at_nano) / 1_000_000);
    let timestamp_part = time_millis % 10_000;
    let total = cmp::max(10_000, base_score.saturating_mul(10_000)) - timestamp_part;
    i64::try_from(total).map_err(|_| Error::BadEntry(format!("score overflow for {}", entry.id)))
}

/// Indices (in id order) of the winning entries. When nobody scored, every
/// entry wins and the coordinator refunds all participants.
fn winning_indices(entries: &[WeatherEntry], number_of_places_win: i64) -> Vec<usize> {
    let mut ordered = entries.to_vec();
    ordered.sort_by_key(|entry| entry.id);
    let all_zero_scores = entries
        .iter()
        .all(|entry| entry.base_score.is_none() || entry.base_score == Some(0));
    if all_zero_scores && !entries.is_empty() {
        return (0..ordered.len()).collect();
    }
    let mut top_entries: Vec<&WeatherEntry> = entries
        .iter()
        .filter(|entry| entry.score.is_some())
        .collect();
    top_entries.sort_by_key(|entry| cmp::Reverse(entry.score));
    top_entries.truncate(usize::try_from(number_of_places_win).unwrap_or(0));
    top_entries
        .iter()
        .filter_map(|top_entry| ordered.iter().position(|entry| entry.id == top_entry.id))
        .collect()
}

pub fn get_winning_bytes(winners: &[usize]) -> Vec<u8> {
    winners
        .iter()
        .flat_map(|&idx| idx.to_be_bytes())
        .collect::<Vec<u8>>()
}

/// Pairs each event station with its forecast and, when available, its
/// observation. Stations without a forecast are skipped.
fn merge_weather(
    event: &ActiveEvent,
    forecast_data: &[Forecast],
    observation_data: &[Observation],
) -> Result<Vec<Weather>, Error> {
    let mut all_weather: Vec<Weather> = vec![];
    for station_id in &event.locations {
        let Some(forecast) = forecast_data
            .iter()
            .find(|forecast| forecast.station_id == *station_id)
        else {
            continue;
        };
        let observed = observation_data
            .iter()
            .find(|observation| observation.station_id == *station_id)
            .map(Observed::try_from)
            .transpose()?;
        all_weather.push(Weather {
            station_id: station_id.clone(),
            observed,
            forecasted: Forecasted::try_from(forecast)?,
        });
    }
    Ok(all_weather)
}

fn get_key(file_path: &Path) -> Result<SecretKey, anyhow::Error> {
    if file_path.extension().and_then(|s| s.to_str()) != Some("pem") {
        return Err(anyhow!("not a '.pem' file extension"));
    }

    if metadata(file_path).is_ok() {
        read_key(file_path)
    } else {
        let key = SecretKey::new(&mut rand::rng());
        save_key(file_path, key)?;
        Ok(key)
    }
}

fn read_key(file_path: &Path) -> Result<SecretKey, anyhow::Error> {
    let mut file = File::open(file_path)?;
    let mut pem_data = String::new();
    file.read_to_string(&mut pem_data)?;

    let (label, decoded_key) = decode_vec(pem_data.as_bytes()).map_err(|e| anyhow!(e))?;
    if label != "EC PRIVATE KEY" {
        return Err(anyhow!("Invalid key format"));
    }
    let bytes: [u8; 32] = decoded_key
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("Invalid key length: expected 32 bytes"))?;
    Ok(SecretKey::from_byte_array(bytes)?)
}

fn save_key(file_path: &Path, key: SecretKey) -> Result<(), anyhow::Error> {
    let pem = encode_string(
        "EC PRIVATE KEY",
        pem_rfc7468::LineEnding::LF,
        &key.secret_bytes(),
    )
    .map_err(|e| anyhow!("Failed to encode key: {}", e))?;

    let mut file = File::create(file_path)?;
    file.write_all(pem.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::WeatherChoices;
    use dlctix::secp::Scalar;

    fn entry_with(id: Uuid, score: Option<i64>, base_score: Option<i64>) -> WeatherEntry {
        WeatherEntry {
            id,
            event_id: Uuid::now_v7(),
            expected_observations: vec![],
            score,
            base_score,
        }
    }

    #[test]
    fn par_over_and_under_score_against_the_forecast() {
        assert_eq!(points(&ValueOptions::Par, 70, 70), PAR_POINTS);
        assert_eq!(points(&ValueOptions::Par, 70, 71), 0);
        assert_eq!(points(&ValueOptions::Over, 70, 71), OVER_OR_UNDER_POINTS);
        assert_eq!(points(&ValueOptions::Over, 70, 69), 0);
        assert_eq!(points(&ValueOptions::Under, 70, 69), OVER_OR_UNDER_POINTS);
        assert_eq!(
            points_within(&ValueOptions::Par, 0.3, 0.35, 0.1),
            PAR_POINTS
        );
        assert_eq!(points_within(&ValueOptions::Par, 0.3, 0.5, 0.1), 0);
        assert_eq!(
            points_within(&ValueOptions::Under, 50, 40, 5),
            OVER_OR_UNDER_POINTS
        );
    }

    #[test]
    fn earlier_entries_win_ties_and_scores_stay_positive() {
        let earlier = entry_with(Uuid::now_v7(), None, None);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let later = entry_with(Uuid::now_v7(), None, None);
        let earlier_total = total_score(&earlier, 20).unwrap();
        let later_total = total_score(&later, 20).unwrap();
        assert!(earlier_total > later_total);
        assert!(total_score(&earlier, 0).unwrap() > 0);
        assert!(total_score(&later, 20).unwrap() > total_score(&earlier, 10).unwrap());
        let v4 = entry_with(Uuid::from_u128(0x1234), None, None);
        assert!(total_score(&v4, 20).is_err());
    }

    #[test]
    fn winners_follow_score_order_by_stable_entry_index() {
        let a = entry_with(Uuid::now_v7(), Some(10_000), Some(0));
        let b = entry_with(Uuid::now_v7(), Some(30_000), Some(30));
        let c = entry_with(Uuid::now_v7(), Some(20_000), Some(20));
        let entries = vec![c.clone(), a.clone(), b.clone()];
        assert_eq!(winning_indices(&entries, 2), vec![1, 2]);
        assert_eq!(winning_indices(&entries, 5), vec![1, 2, 0]);
        let unscored = vec![
            entry_with(Uuid::now_v7(), Some(10_000), Some(0)),
            entry_with(Uuid::now_v7(), None, None),
        ];
        assert_eq!(winning_indices(&unscored, 1), vec![0, 1]);
        assert!(winning_indices(&[], 1).is_empty());
        assert_eq!(get_winning_bytes(&[1, 2]), {
            let mut bytes = 1usize.to_be_bytes().to_vec();
            bytes.extend(2usize.to_be_bytes());
            bytes
        });
    }

    #[test]
    fn entries_must_fit_the_event_shape() {
        let event: Event = crate::events::CreateEventData::new(
            Scalar::random(&mut rand::rng()).base_point_mul(),
            nostr::key::Keys::generate().public_key(),
            CreateEvent {
                id: Uuid::now_v7(),
                signing_date: OffsetDateTime::now_utc() + time::Duration::hours(3),
                start_observation_date: OffsetDateTime::now_utc() + time::Duration::hours(1),
                end_observation_date: OffsetDateTime::now_utc() + time::Duration::hours(2),
                locations: vec!["KORD".into()],
                number_of_values_per_entry: 1,
                total_allowed_entries: 1,
                number_of_places_win: 1,
                scoring_fields: ScoringField::defaults(),
            },
        )
        .unwrap()
        .into();
        let choice = |station: &str, temp_low: Option<ValueOptions>| WeatherChoices {
            stations: station.into(),
            temp_high: Some(ValueOptions::Par),
            temp_low,
            wind_speed: None,
            wind_direction: None,
            rain_amt: None,
            snow_amt: None,
            humidity: None,
        };
        let valid = AddEventEntry {
            id: Uuid::now_v7(),
            event_id: event.id,
            expected_observations: vec![choice("KORD", None)],
        };
        assert!(validate_event_entry(&valid, &event).is_ok());
        let too_many = AddEventEntry {
            expected_observations: vec![choice("KORD", Some(ValueOptions::Over))],
            ..valid.clone()
        };
        assert!(validate_event_entry(&too_many, &event).is_err());
        let wrong_station = AddEventEntry {
            expected_observations: vec![choice("KSAW", None)],
            ..valid.clone()
        };
        assert!(validate_event_entry(&wrong_station, &event).is_err());
        let v4 = AddEventEntry {
            id: Uuid::from_u128(0x1234),
            ..valid
        };
        assert!(validate_event_entry(&v4, &event).is_err());
    }

    #[test]
    fn keys_round_trip_through_pem_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oracle.pem");
        let created = get_key(&path).unwrap();
        let reloaded = get_key(&path).unwrap();
        assert_eq!(created, reloaded);
        assert!(get_key(&directory.path().join("oracle.key")).is_err());
    }
}
