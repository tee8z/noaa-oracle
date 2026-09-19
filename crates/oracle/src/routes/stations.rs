use axum::{
    Json,
    extract::{Query, State},
};
use core::fmt;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use utoipa::{IntoParams, ToSchema};

use crate::{
    AppError, AppState,
    file_access::FileParams,
    weather_data::{DailyObservation, Forecast, Observation, Station, validate_station_id},
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

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
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
    let (req, stations) = bounded_observation_request(req)?;
    let observations = state.weather_db.observation_data(&req, stations).await?;
    Ok(Json(observations))
}

fn bounded_observation_request(
    mut req: ObservationRequest,
) -> Result<(ObservationRequest, Vec<String>), AppError> {
    let stations = checked_stations(&req.station_ids)?;
    let (start, end) = bounded_window(req.start, req.end, OffsetDateTime::now_utc())?;
    (req.start, req.end) = (Some(start), Some(end));
    Ok((req, stations))
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
