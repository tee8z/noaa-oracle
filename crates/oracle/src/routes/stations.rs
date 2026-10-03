use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
};
use core::fmt;
use futures::stream::{self, StreamExt};
use log::warn;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use utoipa::{IntoParams, ToSchema};

use crate::{
    AppError, AppState,
    cache::Cached,
    file_access::FileParams,
    weather_data::{
        DEFAULT_DAYS, DEFAULT_WINDOW_HOURS, DailyObservation, EligibleStation, Forecast, MAX_DAYS,
        MAX_WINDOW_HOURS, Observation, Station, validate_station_id,
    },
};

/// Stations one public query may name.
pub const MAX_STATIONS: usize = 100;
/// Longest time range one public query may cover.
pub const MAX_WINDOW: Duration = Duration::days(31);
/// Range used when a query gives no bounds.
const DEFAULT_WINDOW: Duration = Duration::days(7);

/// Validates the station list of a public query: 1 to [`MAX_STATIONS`]
/// valid ids.
fn checked_stations(station_ids: &str) -> Result<Vec<String>, AppError> {
    let ids = split_station_ids(station_ids);
    if ids.is_empty() || ids.len() > MAX_STATIONS {
        return Err(AppError::InvalidRequest(format!(
            "station_ids must list between 1 and {MAX_STATIONS} stations"
        )));
    }
    for id in &ids {
        validate_station_id(id)?;
    }
    Ok(ids)
}

/// Fills in missing bounds around `now` and rejects ranges longer than
/// [`MAX_WINDOW`], so a public query never scans all history.
pub fn bounded_window(
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Result<(OffsetDateTime, OffsetDateTime), AppError> {
    let (start, end) = match (start, end) {
        (Some(start), Some(end)) => (start, end),
        (Some(start), None) => (start, start.saturating_add(DEFAULT_WINDOW)),
        (None, Some(end)) => (end.saturating_sub(DEFAULT_WINDOW), end),
        (None, None) => (now - DEFAULT_WINDOW, now),
    };
    if end < start || end - start > MAX_WINDOW {
        return Err(AppError::InvalidRequest(format!(
            "time range must be ordered and at most {} days",
            MAX_WINDOW.whole_days()
        )));
    }
    Ok((start, end))
}

#[utoipa::path(
    get,
    path = "/stations/forecasts",
    params(
        ForecastRequest
    ),
    responses(
        (status = OK, description = "Successfully retrieved forecast data", body = Vec<Forecast>),
        (status = BAD_REQUEST, description = "Times are not in RFC3339 format"),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to retrieved weather data")
    ))]
pub async fn forecasts(
    State(state): State<Arc<AppState>>,
    Query(mut req): Query<ForecastRequest>,
) -> Result<Json<Vec<Forecast>>, AppError> {
    let stations = checked_stations(&req.station_ids)?;
    let now = OffsetDateTime::now_utc();
    // Forecasts look ahead by default.
    let (start, end) = bounded_window(
        req.start.or(Some(now)),
        req.end
            .or(req.start.is_none().then(|| now + DEFAULT_WINDOW)),
        now,
    )?;
    (req.start, req.end) = (Some(start), Some(end));
    if req.generated_start.is_some() || req.generated_end.is_some() {
        let (generated_start, generated_end) =
            bounded_window(req.generated_start, req.generated_end, now)?;
        (req.generated_start, req.generated_end) = (Some(generated_start), Some(generated_end));
    }
    let forecasts = state.weather_db.forecasts_data(&req, stations).await?;
    Ok(Json(forecasts))
}

#[derive(Clone, Serialize, Deserialize, IntoParams)]
pub struct ForecastRequest {
    /// Start of the forecast period (the time being forecast)
    #[serde(with = "time::serde::rfc3339::option")]
    #[serde(default)]
    pub start: Option<OffsetDateTime>,
    /// End of the forecast period (the time being forecast)
    #[serde(with = "time::serde::rfc3339::option")]
    #[serde(default)]
    pub end: Option<OffsetDateTime>,
    /// Start of when the forecast was generated/made
    #[serde(with = "time::serde::rfc3339::option")]
    #[serde(default)]
    pub generated_start: Option<OffsetDateTime>,
    /// End of when the forecast was generated/made
    #[serde(with = "time::serde::rfc3339::option")]
    #[serde(default)]
    pub generated_end: Option<OffsetDateTime>,
    pub station_ids: String,
    #[serde(default)]
    pub temperature_unit: TemperatureUnit,
}

impl ForecastRequest {
    pub fn station_ids(&self) -> Vec<String> {
        split_station_ids(&self.station_ids)
    }
}

/// Splits a comma separated list, dropping blanks. Validation happens in
/// the weather layer before any id reaches SQL.
fn split_station_ids(station_ids: &str) -> Vec<String> {
    station_ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

impl From<&ForecastRequest> for FileParams {
    fn from(value: &ForecastRequest) -> Self {
        FileParams {
            start: value.start,
            end: value.end,
            observations: Some(false),
            forecasts: Some(true),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, IntoParams)]
pub struct ObservationRequest {
    #[serde(with = "time::serde::rfc3339::option")]
    #[serde(default)]
    pub start: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[serde(default)]
    pub end: Option<OffsetDateTime>,
    pub station_ids: String,
    #[serde(default)]
    pub temperature_unit: TemperatureUnit,
}

impl ObservationRequest {
    pub fn station_ids(&self) -> Vec<String> {
        split_station_ids(&self.station_ids)
    }
}

impl From<&ObservationRequest> for FileParams {
    fn from(value: &ObservationRequest) -> Self {
        FileParams {
            start: value.start,
            end: value.end,
            observations: Some(true),
            forecasts: Some(false),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TemperatureUnit {
    Celsius,
    #[default]
    Fahrenheit,
}

impl fmt::Display for TemperatureUnit {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TemperatureUnit::Celsius => write!(f, "celsius"),
            TemperatureUnit::Fahrenheit => write!(f, "fahrenheit"),
        }
    }
}

#[utoipa::path(
    get,
    path = "/stations/observations",
    params(
        ObservationRequest
    ),
    responses(
        (status = OK, description = "Successfully retrieved observation data", body = Vec<Observation>),
        (status = BAD_REQUEST, description = "Times are not in RFC3339 format"),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to retrieved weather data")
    ))]
pub async fn observations(
    State(state): State<Arc<AppState>>,
    Query(req): Query<ObservationRequest>,
) -> Result<Json<Vec<Observation>>, AppError> {
    // Windows relative to now differ on every request; only fixed ones are
    // worth keeping.
    let fixed = req.start.is_some() && req.end.is_some();
    let (req, stations) = bounded_observation_request(req)?;
    if !fixed {
        return Ok(Json(
            state.weather_db.observation_data(&req, stations).await?,
        ));
    }
    let key = ObservationKey::new(&req, stations);
    let observations = match state.cached_observations(&key) {
        Cached::Fresh(observations) => observations,
        Cached::Stale {
            value: observations,
            refresh,
        } => {
            if refresh {
                let task_state = state.clone();
                state.spawn(async move {
                    let _ = build_observations(&task_state, key).await;
                });
            }
            observations
        }
        Cached::Missing => build_observations(&state, key).await?,
    };
    Ok(Json(observations.as_ref().clone()))
}

/// The stations, window and unit of a `/stations/observations` request
/// with a fixed window. The coordinator asks for the same ones over and
/// over while a competition runs, and the answer only changes with new
/// data, so it is kept (see [`AppState::cached_observations`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObservationKey {
    /// Sorted, without repeats.
    stations: Vec<String>,
    start: OffsetDateTime,
    end: OffsetDateTime,
    unit: TemperatureUnit,
}

impl ObservationKey {
    /// The key of a bounded request for `stations`.
    fn new(req: &ObservationRequest, mut stations: Vec<String>) -> Self {
        stations.sort_unstable();
        stations.dedup();
        Self {
            stations,
            start: req.start.unwrap_or(OffsetDateTime::UNIX_EPOCH),
            end: req.end.unwrap_or(OffsetDateTime::UNIX_EPOCH),
            unit: req.temperature_unit,
        }
    }

    fn request(&self) -> ObservationRequest {
        ObservationRequest {
            start: Some(self.start),
            end: Some(self.end),
            station_ids: self.stations.join(","),
            temperature_unit: self.unit,
        }
    }
}

/// Reads the aggregates for `key` and keeps them.
async fn build_observations(
    state: &Arc<AppState>,
    key: ObservationKey,
) -> Result<Arc<Vec<Observation>>, crate::weather_data::Error> {
    let generation = state.data_generation();
    match state
        .weather_db
        .observation_data(&key.request(), key.stations.clone())
        .await
    {
        Ok(observations) => {
            let observations = Arc::new(observations);
            state.cache_observations(key, observations.clone(), generation);
            Ok(observations)
        }
        Err(error) => {
            warn!("cannot read observations for {:?}: {error}", key.stations);
            state.observations_refresh_failed(&key);
            Err(error)
        }
    }
}

/// Observation aggregates rebuilt at once by the warmer.
const WARM_CONCURRENCY: usize = 2;

/// Rebuilds the observation aggregates asked for lately, so the next
/// request after new data finds them current.
pub async fn warm_observations(state: &Arc<AppState>) {
    stream::iter(state.recent_observations())
        .for_each_concurrent(WARM_CONCURRENCY, |key| async move {
            let _ = build_observations(state, key).await;
        })
        .await;
}

fn bounded_observation_request(
    mut req: ObservationRequest,
) -> Result<(ObservationRequest, Vec<String>), AppError> {
    let stations = checked_stations(&req.station_ids)?;
    let (start, end) = bounded_window(req.start, req.end, OffsetDateTime::now_utc())?;
    (req.start, req.end) = (Some(start), Some(end));
    Ok((req, stations))
}

#[derive(Deserialize, IntoParams)]
pub struct QualityMetrics {
    /// Comma-separated metrics (such as `temp_high,wind_speed`) whose
    /// report problems to count. Every metric when absent.
    #[serde(default)]
    pub metrics: Option<String>,
}

/// Quality counts use the same row selection and validation rules as settlement.
#[utoipa::path(
    get,
    path = "/stations/observation-quality",
    params(ObservationRequest, QualityMetrics),
    responses(
        (status = OK, description = "Quality counts for the observation window", body = crate::weather_data::ObservationQuality),
        (status = BAD_REQUEST, description = "Invalid station list or time window"),
        (status = SERVICE_UNAVAILABLE, description = "Quality verification unavailable")
    )
)]
pub async fn observation_quality(
    State(state): State<Arc<AppState>>,
    Query(mut req): Query<ObservationRequest>,
    Query(quality): Query<QualityMetrics>,
) -> Result<Json<crate::weather_data::ObservationQuality>, AppError> {
    let stations = checked_stations(&req.station_ids)?;
    let (start, end) = bounded_window(req.start, req.end, OffsetDateTime::now_utc())?;
    (req.start, req.end) = (Some(start), Some(end));
    let metrics: Vec<String> = quality
        .metrics
        .iter()
        .flat_map(|metrics| metrics.split(','))
        .map(str::trim)
        .filter(|metric| !metric.is_empty())
        .map(String::from)
        .collect();
    Ok(Json(
        state
            .weather_db
            .observation_quality(&req, stations, &metrics)
            .await?,
    ))
}

#[utoipa::path(
    get,
    path = "/stations/daily-observations",
    params(
        ObservationRequest
    ),
    responses(
        (status = OK, description = "Successfully retrieved daily observation data", body = Vec<DailyObservation>),
        (status = BAD_REQUEST, description = "Times are not in RFC3339 format"),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to retrieved weather data")
    ))]
pub async fn daily_observations(
    State(state): State<Arc<AppState>>,
    Query(req): Query<ObservationRequest>,
) -> Result<Json<Vec<DailyObservation>>, AppError> {
    let (req, stations) = bounded_observation_request(req)?;
    let observations = state.weather_db.daily_observations(&req, stations).await?;
    Ok(Json(observations))
}

#[utoipa::path(
    get,
    path = "/stations",
    responses(
        (status = OK, description = "Successfully retrieved weather stations", body = Vec<Station>),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to retrieved weather stations from data")
    ))]
pub async fn get_stations(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Station>>, AppError> {
    Ok(Json(state.stations().await?.as_ref().clone()))
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct EligibleRequest {
    /// Full UTC days to judge, ending with yesterday: 1 to 31, 3 by default.
    pub days: Option<u32>,
    /// Length of the competition window in hours: 1 to 24, 24 by default.
    pub window_hours: Option<u32>,
}

impl EligibleRequest {
    /// The days and window hours to judge, defaults filled in.
    fn checked(&self) -> Result<(u32, u32), AppError> {
        let days = self.days.unwrap_or(DEFAULT_DAYS);
        if !(1..=MAX_DAYS).contains(&days) {
            return Err(AppError::InvalidRequest(format!(
                "days must be between 1 and {MAX_DAYS}"
            )));
        }
        let window_hours = self.window_hours.unwrap_or(DEFAULT_WINDOW_HOURS);
        if !(1..=MAX_WINDOW_HOURS).contains(&window_hours) {
            return Err(AppError::InvalidRequest(format!(
                "window_hours must be between 1 and {MAX_WINDOW_HOURS}"
            )));
        }
        Ok((days, window_hours))
    }
}

/// Stations whose reports would have sampled every window of
/// `window_hours` on all but one in ten of the last `days` full UTC days (and always all but one), by the
/// rule settlement applies, that reported within the last 3 hours, and
/// whose newest forecast runs at least `window_hours` past now. Days on
/// which most stations missed a window were collection outages and are not
/// checked. Lists are judged again after each collection run and at least
/// every 10 minutes; requests get the list judged last.
#[utoipa::path(
    get,
    path = "/stations/eligible",
    params(EligibleRequest),
    responses(
        (status = OK, description = "Stations eligible for a competition starting now, by station id", body = Vec<EligibleStation>),
        (status = BAD_REQUEST, description = "Unknown query parameter, or days or window_hours out of range"),
        (status = SERVICE_UNAVAILABLE, description = "Eligibility unavailable")
    ))]
pub async fn eligible_stations(
    State(state): State<Arc<AppState>>,
    query: Result<Query<EligibleRequest>, QueryRejection>,
) -> Result<Json<Vec<EligibleStation>>, AppError> {
    let Query(req) = query.map_err(|rejection| AppError::InvalidRequest(rejection.body_text()))?;
    let (days, window_hours) = req.checked()?;
    let stations = state.eligible_stations(days, window_hours).await?;
    Ok(Json(stations.as_ref().clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn public_queries_get_bounded_windows() {
        let now = datetime!(2030-01-10 00:00 UTC);
        assert_eq!(
            bounded_window(None, None, now).unwrap(),
            (now - DEFAULT_WINDOW, now)
        );
        let start = datetime!(2030-01-01 00:00 UTC);
        assert_eq!(
            bounded_window(Some(start), None, now).unwrap(),
            (start, start + DEFAULT_WINDOW)
        );
        assert!(bounded_window(Some(start), Some(start + MAX_WINDOW), now).is_ok());
        assert!(
            bounded_window(
                Some(start),
                Some(start + MAX_WINDOW + Duration::SECOND),
                now
            )
            .is_err()
        );
        assert!(bounded_window(Some(now), Some(start), now).is_err());
    }

    #[test]
    fn eligibility_queries_have_bounded_days_and_windows() {
        let request = |days, window_hours| EligibleRequest { days, window_hours };
        assert_eq!(request(None, None).checked().unwrap(), (3, 24));
        assert_eq!(request(Some(1), Some(1)).checked().unwrap(), (1, 1));
        assert_eq!(request(Some(31), Some(24)).checked().unwrap(), (31, 24));
        for (days, window_hours) in [(0, 24), (32, 24), (30, 0), (30, 25)] {
            assert!(
                request(Some(days), Some(window_hours)).checked().is_err(),
                "{days} days, {window_hours} hours"
            );
        }
    }

    #[test]
    fn observation_keys_ignore_station_order_and_repeats() {
        let request = ObservationRequest {
            start: Some(datetime!(2030-01-01 00:00 UTC)),
            end: Some(datetime!(2030-01-02 00:00 UTC)),
            station_ids: "KSAW,KORD,KSAW".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let key = ObservationKey::new(&request, request.station_ids());
        assert_eq!(
            key,
            ObservationKey::new(&request, vec!["KORD".into(), "KSAW".into()])
        );
        assert_eq!(key.request().station_ids, "KORD,KSAW");
        let celsius = ObservationRequest {
            temperature_unit: TemperatureUnit::Celsius,
            ..request.clone()
        };
        assert_ne!(key, ObservationKey::new(&celsius, celsius.station_ids()));
    }

    #[test]
    fn public_queries_name_a_bounded_set_of_valid_stations() {
        assert_eq!(
            checked_stations(" KORD, KSAW ,").unwrap(),
            vec!["KORD", "KSAW"]
        );
        assert!(checked_stations("").is_err());
        assert!(checked_stations("KORD,bad'id").is_err());
        let many = vec!["KORD"; MAX_STATIONS + 1].join(",");
        assert!(checked_stations(&many).is_err());
    }
}
