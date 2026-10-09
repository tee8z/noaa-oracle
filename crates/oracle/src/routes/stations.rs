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
    heavy::release_freed_memory_now_and_then,
    routes::Audience,
    weather_data::{
        DEFAULT_DAYS, DEFAULT_WINDOW_HOURS, DERIVED_WINDOW, DailyObservation, EligibleStation,
        Forecast, MAX_WINDOW_HOURS, Observation, Station, forecast_generated_window,
        validate_station_id,
    },
};

/// Stations one public query may name.
pub const MAX_STATIONS: usize = 100;
/// Longest time range one public query may cover.
pub const MAX_WINDOW: Duration = Duration::days(31);
/// Range used when a query gives no bounds.
const DEFAULT_WINDOW: Duration = Duration::days(7);
/// How far back the forecast issues a public query reads may begin: as far
/// as query-ready copies of the published files reach. Older issues would
/// be read from the published files, seconds and a gigabyte or more per
/// request; they remain downloadable from `/files`.
pub const PUBLIC_FORECAST_HISTORY: Duration = DERIVED_WINDOW;

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

/// Forecasts issued in the last 10 days (`generated_start` and
/// `generated_end`, or by default the week of issues before `end` or now,
/// whichever is earlier). Older issues are in the archive files (`/files`).
#[utoipa::path(
    get,
    path = "/stations/forecasts",
    params(
        ForecastRequest
    ),
    responses(
        (status = OK, description = "Successfully retrieved forecast data", body = Vec<Forecast>),
        (status = BAD_REQUEST, description = "Times are not in RFC3339 format, a range is longer than 31 days, or the issues read begin more than 10 days ago"),
        (status = SERVICE_UNAVAILABLE, description = "Weather queries are busy; retry after the Retry-After delay"),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to retrieved weather data")
    ))]
pub async fn forecasts(
    audience: Audience,
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
    if audience == Audience::Public {
        checked_forecast_history(&req, now)?;
    }
    let forecasts = state.weather_db.forecasts_data(&req, stations).await?;
    Ok(Json(forecasts))
}

/// Rejects a public forecast query whose issues begin more than
/// [`PUBLIC_FORECAST_HISTORY`] before `now`. `req` has its bounds filled in.
fn checked_forecast_history(req: &ForecastRequest, now: OffsetDateTime) -> Result<(), AppError> {
    let (issued_from, _) = forecast_generated_window(req, now);
    if issued_from < now - PUBLIC_FORECAST_HISTORY {
        return Err(AppError::InvalidRequest(format!(
            "forecasts issued in the last {} days only: generated_start, or end when \
             generated_start is absent, reaches too far back; older forecasts are in the \
             archive files listed at /files",
            PUBLIC_FORECAST_HISTORY.whole_days()
        )));
    }
    Ok(())
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
            // Rebuilt only on a heavy turn free now (see `crate::heavy`); a
            // later request retries otherwise.
            if refresh {
                match state.heavy().try_turn() {
                    Some(turn) => {
                        let task_state = state.clone();
                        state.spawn(async move {
                            let _turn = turn;
                            // Warming may have rebuilt it since the reader looked.
                            if !matches!(task_state.cached_observations(&key), Cached::Fresh(_)) {
                                let _ = build_observations(&task_state, key).await;
                            }
                        });
                    }
                    None => state.observations_refresh_failed(&key),
                }
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
    pub(crate) fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.stations.capacity() * std::mem::size_of::<String>()
            + self.stations.iter().map(String::capacity).sum::<usize>()
    }

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

/// Observation aggregates the warmer rebuilds at once, each on a heavy turn
/// of its own. An aggregate is a single query, so this is at most two query
/// working sets, within [`HEAVY_TURNS`](crate::heavy::HEAVY_TURNS); forecast
/// details and weather views run several queries each and are warmed one at
/// a time.
const WARM_CONCURRENCY: usize = 2;

/// Rebuilds the observation aggregates asked for lately, so the next
/// request after new data finds them current. Each takes a heavy turn, so
/// a processing pass waits for at most the ones being built.
pub async fn warm_observations(state: &Arc<AppState>) {
    stream::iter(state.recent_observations())
        .for_each_concurrent(WARM_CONCURRENCY, |key| async move {
            let Some(_turn) = state.background_turn().await else {
                return;
            };
            let _ = build_observations(state, key).await;
            release_freed_memory_now_and_then();
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
    /// Full UTC days to judge, ending with yesterday: 1 to 3, 3 by default.
    pub days: Option<u32>,
    /// Length of the competition window in hours: 1 to 24, 24 by default.
    pub window_hours: Option<u32>,
}

impl EligibleRequest {
    /// The days and window hours to judge, defaults filled in. At most
    /// `max_days` of history (see [`Audience::max_eligible_days`]).
    fn checked(&self, max_days: u32) -> Result<(u32, u32), AppError> {
        let days = self.days.unwrap_or(DEFAULT_DAYS);
        if !(1..=max_days).contains(&days) {
            return Err(AppError::InvalidRequest(format!(
                "days must be between 1 and {max_days}"
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

/// Stations whose recent full UTC days and latest rolling competition window
/// meet settlement's sampling rule, with a forecast through the window.
/// All days must pass for histories under ten days; longer histories allow
/// one imperfect day per ten. Collection outages remain missing evidence.
/// Reports must be newer than 90 minutes and judgments newer than 20 minutes.
/// Lists refresh after collection and every 10 minutes; expired entries are omitted.
#[utoipa::path(
    get,
    path = "/stations/eligible",
    params(EligibleRequest),
    responses(
        (status = OK, description = "Stations eligible for a competition starting now, by station id", body = Vec<EligibleStation>),
        (status = BAD_REQUEST, description = "Unknown query parameter, or days or window_hours out of range"),
        (status = SERVICE_UNAVAILABLE, description = "Eligibility unavailable, or busy; retry after the Retry-After delay")
    ))]
pub async fn eligible_stations(
    audience: Audience,
    State(state): State<Arc<AppState>>,
    query: Result<Query<EligibleRequest>, QueryRejection>,
) -> Result<Json<Vec<EligibleStation>>, AppError> {
    let Query(req) = query.map_err(|rejection| AppError::InvalidRequest(rejection.body_text()))?;
    let (days, window_hours) = req.checked(audience.max_eligible_days())?;
    let stations = state.eligible_stations(days, window_hours).await?;
    Ok(Json(
        stations
            .iter()
            .filter(|station| station.current(window_hours, OffsetDateTime::now_utc()))
            .cloned()
            .collect(),
    ))
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
        let operator = Audience::Operator.max_eligible_days();
        assert_eq!(request(None, None).checked(operator).unwrap(), (3, 24));
        assert_eq!(request(Some(1), Some(1)).checked(operator).unwrap(), (1, 1));
        assert_eq!(
            request(Some(31), Some(24)).checked(operator).unwrap(),
            (31, 24)
        );
        for (days, window_hours) in [(0, 24), (32, 24), (30, 0), (30, 25)] {
            assert!(
                request(Some(days), Some(window_hours))
                    .checked(operator)
                    .is_err(),
                "{days} days, {window_hours} hours"
            );
        }
    }

    #[test]
    fn public_eligibility_queries_judge_only_the_days_read_ahead() {
        let request = |days| EligibleRequest {
            days,
            window_hours: Some(24),
        };
        let public = Audience::Public.max_eligible_days();
        assert_eq!(public, crate::weather_data::PRECOMPUTED_DAYS);
        for days in [None, Some(1), Some(2), Some(3)] {
            assert!(request(days).checked(public).is_ok(), "{days:?}");
        }
        for days in [4, 7, 30, 31] {
            let error = request(Some(days)).checked(public).unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid request: days must be between 1 and 3"
            );
        }
    }

    #[test]
    fn public_forecast_queries_read_only_recent_issues() {
        let now = datetime!(2030-01-20 12:00 UTC);
        let request = |start, end, generated_start, generated_end| ForecastRequest {
            start: Some(start),
            end: Some(end),
            generated_start,
            generated_end,
            station_ids: "KORD".into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let tomorrow = now + Duration::DAY;
        // The entry form and discovery: tomorrow, issues of the last week.
        assert!(checked_forecast_history(&request(now, tomorrow, None, None), now).is_ok());
        // A competition that started two days ago, issues of the week before it.
        let started = now - Duration::days(2);
        let baseline = request(
            started,
            started + Duration::DAY,
            Some(started - Duration::days(7)),
            Some(started - Duration::NANOSECOND),
        );
        assert!(checked_forecast_history(&baseline, now).is_ok());
        // Issues from beyond the query-ready copies.
        let old = now - PUBLIC_FORECAST_HISTORY - Duration::SECOND;
        assert!(
            checked_forecast_history(&request(now, tomorrow, Some(old), Some(now)), now).is_err()
        );
        // A past window without issue bounds reads the week before its end.
        let past = now - Duration::days(4);
        assert!(
            checked_forecast_history(&request(past - Duration::DAY, past, None, None), now)
                .is_err()
        );
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
