use ::serde::Deserialize;
use axum::{
    extract::{Query, State},
    Json,
};
use core::fmt;
use serde::Serialize;
use std::sync::Arc;
use time::OffsetDateTime;
use utoipa::{IntoParams, ToSchema};

use crate::{AppError, AppState, FileParams, Forecast, Observation, Station};

#[utoipa::path(
    get,
    path = "stations/forecasts",
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
    Query(req): Query<ForecastRequest>,
) -> Result<Json<Vec<Forecast>>, AppError> {
    let forecasts = state
        .weather_db
        .forecasts_data(&req, req.station_ids())
        .await?;

    Ok(Json(forecasts))
}

#[derive(Clone, Serialize, Deserialize, IntoParams)]
pub struct ForecastRequest {
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

impl ForecastRequest {
    pub fn station_ids(&self) -> Vec<String> {
        self.station_ids
            .split(',')
            .map(|id| id.to_owned())
            .collect()
    }
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
        self.station_ids
            .split(',')
            .map(|id| id.to_owned())
            .collect()
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

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, ToSchema)]
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
    path = "stations/observations",
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
    let observations = state
        .weather_db
        .observation_data(&req, req.station_ids())
        .await?;

    Ok(Json(observations))
}

#[utoipa::path(
    get,
    path = "stations",
    responses(
        (status = OK, description = "Successfully retrieved weather stations", body = Vec<Station>),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to retrieved weather stations from data")
    ))]
pub async fn get_stations(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Station>>, AppError> {
    let stations: Vec<Station> = state.weather_db.stations().await?;
    Ok(Json(stations))
}
