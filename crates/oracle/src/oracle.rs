use crate::{
    weather_data, ActiveEvent, AddEventEntry, CreateEvent, CreateEventData, Event, EventData,
    EventFilter, EventStatus, EventSummary, Forecast, ForecastRequest, Observation,
    ObservationRequest, SignEvent, TemperatureUnit, ValueOptions, Weather, WeatherData,
    WeatherEntry,
};
use anyhow::anyhow;
use base64::{engine::general_purpose, Engine};
use dlctix::{
    attestation_locking_point, attestation_secret,
    musig2::secp256k1::{rand, PublicKey, Secp256k1, SecretKey},
    secp::{MaybePoint, Point},
};
use duckdb::arrow::datatypes::ArrowNativeType;
use log::{debug, error, info, warn};
use nostr_sdk::{key::Keys, nips::nip19::ToBech32, PublicKey as NostrPublicKey};
use pem_rfc7468::{decode_vec, encode_string};
use serde::Serialize;
use std::{
    cmp,
    fs::{metadata, File},
    io::{Read, Write},
    path::Path,
    sync::Arc,
};
use thiserror::Error;
use time::{Duration, OffsetDateTime};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Error, Debug, Serialize, ToSchema)]
pub enum Error {
    #[error("{0}")]
    NotFound(String),
    #[schema(value_type = String)]
    #[error("Failed to get key: {0}")]
    ValidateKey(
        #[serde(skip)]
        #[from]
        anyhow::Error,
    ),
    #[error("Must have at least one outcome: {0}")]
    MinOutcome(String),
    #[error("Event maturity epoch must be in the future: {0}")]
    EventMaturity(String),
    #[schema(value_type = String)]
    #[error("Failed to convert private key into nostr keys: {0}")]
    ConvertKey(
        #[serde(skip)]
        #[from]
        nostr_sdk::key::Error,
    ),
    #[schema(value_type = String)]
    #[error("Failed to convert public key into nostr base32 format: {0}")]
    Base32Key(
        #[serde(skip)]
        #[from]
        nostr_sdk::nips::nip19::Error,
    ),
    #[schema(value_type = String)]
    #[error("Failed to query datasource: {0}")]
    DataQuery(
        #[serde(skip)]
        #[from]
        duckdb::Error,
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
    #[schema(value_type = String)]
    #[error("Failed to validate message: {0}")]
    Validation(
        #[serde(skip)]
        #[from]
        serde_json::Error,
    ),
}

pub struct Oracle {
    event_data: Arc<EventData>,
    weather_data: Arc<dyn WeatherData>, //need this to be a trait so I can mock the weather data
    private_key: SecretKey,
    public_key: PublicKey,
}

impl Oracle {
    pub async fn new(
        event_data: Arc<EventData>,
        weather_data: Arc<dyn WeatherData>,
        private_key_file_path: &String,
    ) -> Result<Self, Error> {
        let secret_key = get_key(private_key_file_path)?;
        let secp = Secp256k1::new();
        let public_key = secret_key.public_key(&secp);
        let oracle = Self {
            event_data,
            weather_data,
            private_key: secret_key,
            public_key,
        };
        oracle.validate_oracle_metadata().await?;
        Ok(oracle)
    }

    pub async fn validate_oracle_metadata(&self) -> Result<(), Error> {
        let stored_public_key = match self.event_data.get_stored_public_key().await {
            Ok(key) => key,
            Err(duckdb::Error::QueryReturnedNoRows) => {
                self.add_meta_data().await?;
                return Ok(());
            }
            Err(e) => return Err(Error::DataQuery(e)),
        };
        if stored_public_key != self.public_key.x_only_public_key().0 {
            return Err(Error::MismatchPubkey(format!(
                "stored_pubkey: {:?} pem_pubkey: {:?}",
                stored_public_key,
                self.public_key()
            )));
        }
        Ok(())
    }

    async fn add_meta_data(&self) -> Result<(), Error> {
        self.event_data
            .add_oracle_metadata(self.public_key.x_only_public_key().0)
            .await
            .map_err(Error::DataQuery)
    }

    pub fn raw_public_key(&self) -> PublicKey {
        self.public_key
    }

    pub fn raw_private_key(&self) -> SecretKey {
        self.private_key
    }

    pub fn public_key(&self) -> String {
        let key = Point::from(self.public_key).serialize();
        general_purpose::STANDARD.encode(key)
    }

    pub fn npub(&self) -> Result<String, Error> {
        let secret_key = self.private_key.display_secret().to_string();
        let keys = Keys::parse(&secret_key)?;

        Ok(keys.public_key().to_bech32()?)
    }

    pub async fn list_events(&self, filter: EventFilter) -> Result<Vec<EventSummary>, Error> {
        // TODO: add filter/pagination etc.
        // filter on active event/completed event/time range of event
        // if we're not careful, this endpoint might bring down the whole server
        // just due to the amount of data that can come out of it
        self.event_data
            .filtered_list_events(filter)
            .await
            .map_err(Error::DataQuery)
    }

    pub async fn get_event(&self, id: &Uuid) -> Result<Event, Error> {
        match self.event_data.get_event(id).await {
            Ok(event_data) => Ok(event_data),
            Err(duckdb::Error::QueryReturnedNoRows) => {
                Err(Error::NotFound(format!("event with id {} not found", id)))
            }
            Err(e) => Err(Error::DataQuery(e)),
        }
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
        if event.total_allowed_entries > 25 {
            return Err(Error::BadEvent(anyhow!(
                "Max number of allowed entries the oracle can watch is 25"
            )));
        }
        if event.number_of_places_win > 5 {
            return Err(Error::BadEvent(anyhow!(
                "Max number of allowed ranks in an event that can win is 5, requested: {}",
                event.number_of_places_win
            )));
        }

        let oracle_event = CreateEventData::new(
            Point::from(self.raw_public_key()),
            coordinator_pubkey,
            event,
        )
        .map_err(Error::BadEvent)?;
        self.event_data
            .add_event(oracle_event)
            .await
            .map_err(Error::DataQuery)
    }

    pub async fn add_event_entries(
        &self,
        nostr_pubkey: NostrPublicKey,
        event_id: Uuid,
        entries: Vec<AddEventEntry>,
    ) -> Result<Vec<WeatherEntry>, Error> {
        let nostr_pubkey = nostr_pubkey.to_bech32()?;
        if event_id.get_version_num() != 7 {
            return Err(Error::BadEntry(format!(
                "Client needs to provide a valid Uuidv7 for event id {}",
                event_id
            )));
        }
        let event = match self.event_data.get_event(&event_id).await {
            Ok(event_data) => Ok(event_data),
            Err(duckdb::Error::QueryReturnedNoRows) => Err(Error::NotFound(format!(
                "event with id {} not found",
                &event_id
            ))),
            Err(e) => Err(Error::DataQuery(e)),
        }?;
        if event.entries.len() > 0 {
            return Err(Error::BadEntry(format!(
                "event {} already has entries, no more entries are allowed",
                event.id
            )));
        }
        if event.total_allowed_entries.as_usize() != entries.len() {
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
        let mut weather_entry: Vec<WeatherEntry> = vec![];
        for entry in entries {
            if entry.event_id != event_id {
                return Err(Error::BadEntry(format!(
                    "Client add entries to be for this event {}, entry {} has the wrong event id {}",
                    event_id, entry.id, entry.event_id
                )));
            }
            self.validate_event_entry(entry.clone(), event.clone())
                .await?;
            weather_entry.push(entry.into());
        }
        self.event_data
            .add_event_entries(weather_entry.clone())
            .await
            .map_err(Error::DataQuery)?;

        Ok(weather_entry)
    }

    async fn validate_event_entry(&self, entry: AddEventEntry, event: Event) -> Result<(), Error> {
        if entry.id.get_version_num() != 7 {
            return Err(Error::BadEntry(format!(
                "Client needs to provide a valid Uuidv7 for entry id {}",
                entry.id
            )));
        }

        let mut choice_count = 0;
        for weather_choice in &entry.expected_observations {
            if weather_choice.temp_high.is_some() {
                choice_count += 1;
            }
            if weather_choice.temp_low.is_some() {
                choice_count += 1;
            }
            if weather_choice.wind_speed.is_some() {
                choice_count += 1;
            }

            if choice_count > event.number_of_values_per_entry {
                return Err(Error::BadEntry(format!(
                    "entry_id {0} not valid, too many value choices, max allowed {1} but got {2}",
                    entry.id, event.number_of_values_per_entry, choice_count
                )));
            }
        }

        let locations_choose: Vec<String> = entry
            .expected_observations
            .clone()
            .iter()
            .map(|weather_vals| weather_vals.stations.clone())
            .collect();
        let all_valid_locations = locations_choose
            .iter()
            .all(|choose| event.locations.contains(choose));
        if !all_valid_locations {
            return Err(Error::BadEntry(format!(
                "entry_id {0} not valid, choose locations not in the even",
                entry.id
            )));
        }
        Ok(())
    }

    pub async fn get_running_events(&self) -> Result<Vec<ActiveEvent>, Error> {
        match self.event_data.get_active_events().await {
            Ok(event_data) => Ok(event_data),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(vec![]),
            Err(e) => Err(Error::DataQuery(e)),
        }
    }

    pub async fn get_event_entry(
        &self,
        event_id: &Uuid,
        entry_id: &Uuid,
    ) -> Result<WeatherEntry, Error> {
        match self.event_data.get_weather_entry(event_id, entry_id).await {
            Ok(event_data) => Ok(event_data),
            Err(duckdb::Error::QueryReturnedNoRows) => Err(Error::NotFound(format!(
                "entry with id {} not found for event {}",
                &entry_id, &event_id
            ))),
            Err(e) => Err(Error::DataQuery(e)),
        }
    }

    pub async fn etl_data(&self, etl_process_id: usize) -> Result<(), Error> {
        // NOTE: Making the assumption the number of active events will remain small, maybe 10 at most for now,
        // Also assuming it's okay to have duplicate location weather reading rows for now (if this becomes a problem we will need to de-dup)
        info!(" etl_process_id {}, starting etl process", etl_process_id);
        debug!(" etl_process_id {}, getting running events", etl_process_id);
        let events_to_update = self.get_running_events().await?;
        debug!(
            " etl_process_id {}, completed getting running events",
            etl_process_id
        );
        // 1) update weather readings
        debug!(
            " etl_process_id {}, updating weather readings",
            etl_process_id
        );
        self.update_event_weather_data(etl_process_id, events_to_update.clone())
            .await?;
        debug!(
            " etl_process_id {}, completed updating weather readings",
            etl_process_id
        );
        debug!(" etl_process_id {}, getting active events", etl_process_id);
        // 2) update entry scores for running & completed events
        let events: Vec<ActiveEvent> = events_to_update
            .iter()
            .filter(|entry| {
                (entry.status == EventStatus::Running || entry.status == EventStatus::Completed)
                    && entry.attestation.is_none()
            })
            .cloned()
            .collect();
        debug!(
            " etl_process_id {}, completed getting active events",
            etl_process_id
        );
        debug!(
            " etl_process_id {}, updating entry scores for active events",
            etl_process_id
        );
        self.update_active_events_entry_scores(etl_process_id, events)
            .await?;
        debug!(
            " etl_process_id {}, completed entry scores for active events",
            etl_process_id
        );
        debug!(" etl_process_id {}, getting events to sign", etl_process_id);
        // 3) sign results for events that are completed and need it
        let events_to_sign: Vec<Uuid> = events_to_update
            .iter()
            .filter(|event| event.status == EventStatus::Completed && event.attestation.is_none())
            .map(|event| event.id)
            .collect();
        debug!(
            " etl_process_id {}, completed getting events to sign",
            etl_process_id
        );
        if events_to_sign.is_empty() {
            info!(
                " etl_process_id {}, no events to sign, completed etl process",
                etl_process_id
            );
            return Ok(());
        }
        debug!(
            " etl_process_id {}, adding oracle signature to events",
            etl_process_id
        );
        self.add_oracle_signature(etl_process_id, events_to_sign)
            .await?;
        debug!(
            " etl_process_id {}, completed adding oracle signature to events",
            etl_process_id
        );
        info!(" etl_process_id {}, completed etl process", etl_process_id);
        Ok(())
    }

    async fn update_event_weather_data(
        &self,
        etl_process_id: usize,
        events_to_update: Vec<ActiveEvent>,
    ) -> Result<(), Error> {
        for event in events_to_update {
            info!(
                "updating event {} with status {} weather data in process {}",
                event.id, event.status, etl_process_id
            );
            let forecast_data = self.event_forecast_data(&event).await?;
            let weather = if event.observation_date > OffsetDateTime::now_utc() {
                add_only_forecast_data(&event, forecast_data).await?
            } else {
                let observation_data = self.event_observation_data(&event).await?;
                add_forecast_data_and_observation_data(&event, forecast_data, observation_data)
                    .await?
            };
            self.event_data
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
        etl_process_id: usize,
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
        etl_process_id: usize,
        event: ActiveEvent,
    ) -> Result<(), Error> {
        let entries: Vec<WeatherEntry> =
            self.event_data.get_event_weather_entries(&event.id).await?;

        let observation_data = self.event_observation_data(&event).await?;
        let forecast_data = self.event_forecast_data(&event).await?;
        let mut entry_scores: Vec<(Uuid, i64)> = vec![];

        for entry in entries {
            if entry.event_id != event.id {
                warn!("entry {} not in this event {}", entry.id, event.id);
                continue;
            }

            // Score logic, match on Par 2pts, on Over 1pt, on Under 1pt, created_at used as tie breaker (older > newer)
            let mut base_score = 0;
            const OVER_OR_UNDER_POINTS: u64 = 10;
            const PAR_POINTS: u64 = 20;
            let expected_observations = entry.expected_observations.clone();
            let locations = event.locations.clone();
            for location in locations {
                let Some(choice) = expected_observations
                    .iter()
                    .find(|expected| expected.stations == location)
                else {
                    continue;
                };

                let Some(forecast) = forecast_data
                    .iter()
                    .find(|forecast| forecast.station_id == location)
                else {
                    warn!("no forecast found for: {}", location);
                    continue;
                };

                let Some(observation) = observation_data
                    .iter()
                    .find(|observation| observation.station_id == location)
                else {
                    warn!("no observation found for: {}", location);
                    continue;
                };

                if let Some(high_temp) = choice.temp_high.clone() {
                    match high_temp {
                        ValueOptions::Over => {
                            if forecast.temp_high < observation.temp_high.round() as i64 {
                                base_score += OVER_OR_UNDER_POINTS;
                            }
                        }
                        ValueOptions::Par => {
                            if forecast.temp_high == observation.temp_high.round() as i64 {
                                base_score += PAR_POINTS;
                            }
                        }
                        ValueOptions::Under => {
                            if forecast.temp_high > observation.temp_high.round() as i64 {
                                base_score += OVER_OR_UNDER_POINTS;
                            }
                        }
                    }
                }

                if let Some(temp_low) = choice.temp_low.clone() {
                    match temp_low {
                        ValueOptions::Over => {
                            if forecast.temp_low < observation.temp_low.round() as i64 {
                                base_score += OVER_OR_UNDER_POINTS;
                            }
                        }
                        ValueOptions::Par => {
                            if forecast.temp_low == observation.temp_low.round() as i64 {
                                base_score += PAR_POINTS;
                            }
                        }
                        ValueOptions::Under => {
                            if forecast.temp_low > observation.temp_low.round() as i64 {
                                base_score += OVER_OR_UNDER_POINTS;
                            }
                        }
                    }
                }

                if let Some(wind_speed) = choice.wind_speed.clone() {
                    match wind_speed {
                        ValueOptions::Over => {
                            if forecast.wind_speed < observation.wind_speed {
                                base_score += OVER_OR_UNDER_POINTS;
                            }
                        }
                        ValueOptions::Par => {
                            if forecast.wind_speed == observation.wind_speed {
                                base_score += PAR_POINTS;
                            }
                        }
                        ValueOptions::Under => {
                            if forecast.wind_speed > observation.wind_speed {
                                base_score += OVER_OR_UNDER_POINTS;
                            }
                        }
                    }
                }
            }
            let (created_at_secs, created_at_nano) = entry
                .id
                .get_timestamp()
                .expect("UUIDv7 should have timestamp")
                .to_unix();
            let time_millis = (created_at_secs * 1000) + (created_at_nano as u64 / 1_000_000);

            // Scoring logic: score * 10^4 - timestamp
            // Using 4 digits for timestamp (keeping within the 10000 range as before)
            // Limit timestamp to last 4 digits (mod 10000) to maintain consistency with old code
            let timestamp_part = time_millis % 10000;
            // Use this to ensure uniqueness:
            let total_score = (std::cmp::max(10000, base_score * 10000) - timestamp_part) as i64;

            /* With our formula score * 10^4 - timestamp:
            - Higher base scores will still dominate (primary sorting criterion)
            - For equal scores, earlier entries (smaller timestamps) will result in higher total scores
              which means they'll rank higher when sorting in descending order

            This maintains the original constraints:
            - Up to 10,000 entries over 24h with negligible collision risk
            - Scales well for concurrent entry creation
            - Keeps the amount of possible outcomes for the DLC as low as possible
            */

            info!(
                "updating entry {} for event {} to score {} in etl process {}",
                entry.id, event.id, total_score, etl_process_id
            );

            entry_scores.push((entry.id, total_score));
        }

        self.event_data.update_entry_scores(entry_scores).await?;

        Ok(())
    }

    async fn add_oracle_signature(
        &self,
        etl_process_id: usize,
        event_ids: Vec<Uuid>,
    ) -> Result<(), Error> {
        let mut events: Vec<SignEvent> = self.event_data.get_events_to_sign(event_ids).await?;
        info!("events: {:?}", events);
        for event in events.iter_mut() {
            let entries = self.event_data.get_event_weather_entries(&event.id).await?;
            let mut entry_indices = entries.clone();
            // very important, the sort index of the entry should always be the same when getting the outcome
            entry_indices.sort_by_key(|entry| entry.id);

            // Sort by score descending for top 3
            let mut top_entries = entries.clone();
            top_entries.sort_by_key(|entry| cmp::Reverse(entry.score));
            top_entries.truncate(3);

            // Get indices of top 3 in original entry_indices order
            let winners: Vec<usize> = top_entries
                .iter()
                .map(|top_entry| {
                    entry_indices
                        .iter()
                        .position(|entry| entry.id == top_entry.id)
                        .expect("Entry should exist")
                })
                .collect();

            if event.signing_date < OffsetDateTime::now_utc() {
                let winner_bytes: Vec<u8> = get_winning_bytes(winners.clone());

                let nonce_point = event.nonce.base_point_mul();

                let locking_point =
                    attestation_locking_point(self.public_key, nonce_point, &winner_bytes);

                info!("winner_bytes: {:?}", winner_bytes);

                let winners_str = winners
                    .iter()
                    .filter_map(|entry_index| entry_indices.get(*entry_index))
                    .map(|entry| format!("({}, {})", entry.score.unwrap_or_default(), entry.id))
                    .collect::<Vec<String>>()
                    .join(", ");

                let MaybePoint::Valid(_) = locking_point else {
                    // Something went horribly wrong, use the info from this log line to track refunding users based on DLC expiry (we set to 1 week)
                    error!("final result doesn't match any of the possible outcomes: event_id {} winners {} expiry {:?}", event.id, winners_str, event.event_announcement.expiry);

                    return Err(Error::OutcomeNotFound(format!(
                        "event_id {} outcome winners {} expiry {:?}",
                        event.id, winners_str, event.event_announcement.expiry
                    )));
                };

                info!("winners: event_id {} winners {}", event.id, winners_str);

                let attestation = attestation_secret(self.private_key, event.nonce, &winner_bytes);
                event.attestation = Some(attestation);
                self.event_data.update_event_attestation(event).await?;
            }
        }
        info!(
            "completed adding oracle signature to all events that need it in etl process {}",
            etl_process_id
        );
        Ok(())
    }

    async fn event_forecast_data(&self, event: &ActiveEvent) -> Result<Vec<Forecast>, Error> {
        let start_date = event.observation_date;
        // Assumes all events are only a day long, may change in the future
        let end_date = event.observation_date.saturating_add(Duration::days(1));
        // Assumes locations have been sanitized when the event was created
        let station_ids = event.locations.join(",");
        let forecast_requests = ForecastRequest {
            start: Some(start_date),
            end: Some(end_date),
            station_ids: station_ids.clone(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        self.weather_data
            .forecasts_data(&forecast_requests, event.locations.clone())
            .await
            .map_err(Error::WeatherData)
    }

    async fn event_observation_data(&self, event: &ActiveEvent) -> Result<Vec<Observation>, Error> {
        let start_date = event.observation_date;
        // Assumes all events are only a day long, may change in the future
        let end_date = event.observation_date.saturating_add(Duration::days(1));
        let observation_requests = ObservationRequest {
            start: Some(start_date),
            end: Some(end_date),
            station_ids: event.locations.join(","),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        self.weather_data
            .observation_data(&observation_requests, event.locations.clone())
            .await
            .map_err(Error::WeatherData)
    }
}

pub fn get_winning_bytes(winners: Vec<usize>) -> Vec<u8> {
    winners
        .iter()
        .flat_map(|&idx| idx.to_be_bytes())
        .collect::<Vec<u8>>()
}

async fn add_only_forecast_data(
    event: &ActiveEvent,
    forecast_data: Vec<Forecast>,
) -> Result<Vec<Weather>, Error> {
    let mut all_weather: Vec<Weather> = vec![];

    for station_id in event.locations.clone() {
        if let Some(forecast) = forecast_data
            .iter()
            .find(|forecast| forecast.station_id == station_id.clone())
        {
            let weather = Weather {
                station_id: station_id.clone(),
                observed: None,
                forecasted: forecast.try_into().map_err(Error::WeatherData)?,
            };
            all_weather.push(weather);
        }
    }
    Ok(all_weather)
}

async fn add_forecast_data_and_observation_data(
    event: &ActiveEvent,
    forecast_data: Vec<Forecast>,
    observation_data: Vec<Observation>,
) -> Result<Vec<Weather>, Error> {
    let mut all_weather: Vec<Weather> = vec![];

    for station_id in event.locations.clone() {
        if let Some(forecast) = forecast_data
            .iter()
            .find(|forecast| forecast.station_id == station_id.clone())
        {
            let weather = if let Some(observation) = observation_data
                .iter()
                .find(|observation| observation.station_id == station_id.clone())
            {
                Weather {
                    station_id: station_id.clone(),
                    observed: observation
                        .try_into()
                        .map(Some)
                        .map_err(Error::WeatherData)?,
                    forecasted: forecast.try_into().map_err(Error::WeatherData)?,
                }
            } else {
                Weather {
                    station_id: station_id.clone(),
                    observed: None,
                    forecasted: forecast.try_into().map_err(Error::WeatherData)?,
                }
            };
            all_weather.push(weather);
        }
    }
    Ok(all_weather)
}

fn get_key(file_path: &String) -> Result<SecretKey, anyhow::Error> {
    if !is_pem_file(file_path) {
        return Err(anyhow!("not a '.pem' file extension"));
    }

    if metadata(file_path).is_ok() {
        read_key(file_path)
    } else {
        let key = generate_new_key();
        save_key(file_path, key)?;
        Ok(key)
    }
}

fn generate_new_key() -> SecretKey {
    SecretKey::new(&mut rand::thread_rng())
}

fn is_pem_file(file_path: &String) -> bool {
    Path::new(file_path)
        .extension()
        .and_then(|s| s.to_str())
        .map_or(false, |ext| ext == "pem")
}

fn read_key(file_path: &String) -> Result<SecretKey, anyhow::Error> {
    let mut file = File::open(file_path)?;
    let mut pem_data = String::new();
    file.read_to_string(&mut pem_data)?;

    // Decode the PEM content
    let (label, decoded_key) = decode_vec(pem_data.as_bytes()).map_err(|e| anyhow!(e))?;

    // Verify the label
    if label != "EC PRIVATE KEY" {
        return Err(anyhow!("Invalid key format"));
    }

    // Parse the private key
    let secret_key = SecretKey::from_slice(&decoded_key)?;
    Ok(secret_key)
}

fn save_key(file_path: &String, key: SecretKey) -> Result<(), anyhow::Error> {
    let pem = encode_string(
        "EC PRIVATE KEY",
        pem_rfc7468::LineEnding::LF,
        &key.secret_bytes(),
    )
    .map_err(|e| anyhow!("Failed to encode key: {}", e))?;

    // Private key file path needs to end in ".pem"
    let mut file = File::create(file_path)?;
    file.write_all(pem.as_bytes())?;
    Ok(())
}
