//! Read-only planning evidence. A compatible forecast does not promise future
//! observations, collector uptime, station capability, or a settlement outcome.
//!
//! An assessment reads a week of published forecast files, seconds of work
//! and up to a gigabyte, so assessments are kept per question until new
//! data arrives (at most 10 minutes) and built on a heavy turn (see
//! [`crate::heavy`]).

use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};
use time::{Duration, OffsetDateTime, UtcOffset};
use utoipa::{IntoParams, ToSchema};

use crate::{
    AppError, AppState,
    heavy::Turns,
    routes::{ForecastRequest, TemperatureUnit},
    sources::noaa::{
        HUMIDITY, RAIN_AMT, SNOW_AMT, TEMP_HIGH, TEMP_LOW, WIND_DIRECTION, WIND_SPEED,
    },
    weather_data::{ForecastAssessment, ForecastNativeInterval, validate_station_id},
};

const MAX_STATIONS: usize = 20;
const MAX_WINDOW: Duration = Duration::days(7);
const METRICS: [&str; 7] = [
    TEMP_HIGH,
    TEMP_LOW,
    WIND_SPEED,
    WIND_DIRECTION,
    RAIN_AMT,
    SNOW_AMT,
    HUMIDITY,
];

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct WindowCompatibilityRequest {
    #[serde(with = "time::serde::rfc3339")]
    pub start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end: OffsetDateTime,
    /// Between 1 and 20 distinct station IDs, separated by commas.
    pub station_ids: String,
    /// Metric IDs separated by commas. Defaults to temp_high,temp_low,wind_speed.
    pub metrics: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct RequestedWindow {
    #[serde(with = "time::serde::rfc3339")]
    pub start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end: OffsetDateTime,
    pub station_ids: Vec<String>,
    pub metrics: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct MetricCompatibility {
    pub station_id: String,
    pub metric: String,
    pub unit: String,
    pub baseline_available: bool,
    pub baseline: Option<f64>,
    pub reason: Option<String>,
    pub native_intervals: Vec<ForecastNativeInterval>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObservationPlanningStatus {
    /// The requested window has not ended; future measurements do not exist yet.
    Pending,
    /// This endpoint checks forecasts, not observation quality or completeness.
    NotAssessed,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrecipitationCapability {
    /// Recent verified fixed-hour reports exist. Future service is not guaranteed.
    RecentlyObserved,
    /// No verified recent evidence was available. This does not mean unsupported.
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct StationCapability {
    pub station_id: String,
    pub fixed_hour_precipitation_source: PrecipitationCapability,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct WindowCompatibility {
    pub source: String,
    pub requested_window: RequestedWindow,
    #[serde(with = "time::serde::rfc3339")]
    pub evaluated_at: OffsetDateTime,
    pub forecasts: Vec<MetricCompatibility>,
    pub observations: ObservationPlanningStatus,
    pub stations: Vec<StationCapability>,
    /// Always false: planning evidence never authorizes a signature.
    pub settlement_ready: bool,
    /// Present when the optional recent precipitation capability lookup failed.
    pub precipitation_capability_warning: Option<String>,
    pub notice: String,
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::InvalidRequest(message.into())
}

/// One planning question: a window in UTC, its stations and metrics in the
/// order asked, as the answer lists them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlanKey {
    start: OffsetDateTime,
    end: OffsetDateTime,
    stations: Vec<String>,
    metrics: Vec<String>,
}

impl PlanKey {
    /// Retained bytes of a kept assessment, roughly.
    pub(crate) fn estimated_bytes(&self, plan: &Arc<WindowCompatibility>) -> usize {
        let strings = |values: &[String]| {
            std::mem::size_of_val(values) + values.iter().map(String::capacity).sum::<usize>()
        };
        std::mem::size_of::<Self>()
            + std::mem::size_of::<WindowCompatibility>()
            + 2 * (strings(&self.stations) + strings(&self.metrics))
            + plan.forecasts.capacity() * (std::mem::size_of::<MetricCompatibility>() + 256)
            + plan.stations.capacity() * (std::mem::size_of::<StationCapability>() + 16)
            + plan.notice.capacity()
    }
}

/// Whether observations of a window ending at `end` can exist at `now`.
fn observation_status(end: OffsetDateTime, now: OffsetDateTime) -> ObservationPlanningStatus {
    if end > now {
        ObservationPlanningStatus::Pending
    } else {
        ObservationPlanningStatus::NotAssessed
    }
}

impl WindowCompatibilityRequest {
    fn checked(&self) -> Result<(Vec<String>, Vec<String>), AppError> {
        if self.start >= self.end || self.end - self.start > MAX_WINDOW {
            return Err(invalid(
                "start must precede end and the requested window must be at most 7 days",
            ));
        }
        let stations = self
            .station_ids
            .split(',')
            .map(str::trim)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if stations.is_empty()
            || stations.len() > MAX_STATIONS
            || stations.iter().collect::<HashSet<_>>().len() != stations.len()
        {
            return Err(invalid(
                "station_ids must list between 1 and 20 distinct stations",
            ));
        }
        for station in &stations {
            validate_station_id(station)?;
        }
        let metrics = self
            .metrics
            .as_deref()
            .unwrap_or("temp_high,temp_low,wind_speed")
            .split(',')
            .map(str::trim)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if metrics.is_empty()
            || metrics.len() > METRICS.len()
            || metrics.iter().collect::<HashSet<_>>().len() != metrics.len()
            || metrics
                .iter()
                .any(|metric| !METRICS.contains(&metric.as_str()))
        {
            return Err(invalid(
                "metrics must be distinct NOAA metric IDs: temp_high,temp_low,wind_speed,wind_direction,rain_amt,snow_amt,humidity",
            ));
        }
        Ok((stations, metrics))
    }
}

fn metric_compatibility(
    station: &str,
    metric: &str,
    rows: &[ForecastAssessment],
) -> MetricCompatibility {
    let mut matching = rows
        .iter()
        .filter(|row| row.station_id == station && row.metric == metric);
    let row = matching.next();
    let ambiguous = matching.next().is_some();
    let (baseline, reason, native_intervals) = match row {
        Some(row) if !ambiguous => {
            let baseline = row
                .value
                .filter(|value| value.is_finite() && row.reason.is_none());
            let reason =
                if baseline.is_some() {
                    None
                } else {
                    Some(row.reason.clone().unwrap_or_else(|| {
                        "No verified baseline covers this metric and window".into()
                    }))
                };
            (baseline, reason, row.native_intervals.clone())
        }
        _ => (
            None,
            Some(
                "A unique forecast assessment was not available for this station and metric".into(),
            ),
            vec![],
        ),
    };
    let unit = match metric {
        TEMP_HIGH | TEMP_LOW => "fahrenheit",
        WIND_SPEED => "knots",
        WIND_DIRECTION => "degrees_true",
        HUMIDITY => "percent",
        _ => "inches",
    };
    MetricCompatibility {
        station_id: station.into(),
        metric: metric.into(),
        unit: unit.into(),
        baseline_available: baseline.is_some(),
        baseline,
        reason,
        native_intervals,
    }
}

#[utoipa::path(
    get,
    path = "/stations/window-compatibility",
    params(WindowCompatibilityRequest),
    responses(
        (status = OK, description = "Read-only forecast window assessment; never settlement authorization", body = WindowCompatibility),
        (status = BAD_REQUEST, description = "Invalid station, metric, or bounded time window"),
        (status = SERVICE_UNAVAILABLE, description = "Forecast quality could not be assessed"),
        (status = INTERNAL_SERVER_ERROR, description = "Forecast source could not be read")
    )
)]
pub async fn window_compatibility(
    State(state): State<Arc<AppState>>,
    Query(request): Query<WindowCompatibilityRequest>,
) -> Result<Json<WindowCompatibility>, AppError> {
    let (stations, metrics) = request.checked()?;
    let key = PlanKey {
        start: request.start.to_offset(UtcOffset::UTC),
        end: request.end.to_offset(UtcOffset::UTC),
        stations,
        metrics,
    };
    let generation = state.data_generation();
    let builder = state.clone();
    let assessed = key.clone();
    let plan = state
        .plans()
        .get(
            state.heavy(),
            Turns::One,
            key,
            generation,
            move || async move { assess(&builder, assessed).await.map(Arc::new) },
        )
        .await
        .map_err(|error| state.turned_away(error))?;
    // Kept assessments say when they were made; whether observations can
    // exist yet depends on the time asked.
    let mut plan = plan.as_ref().clone();
    plan.observations = observation_status(plan.requested_window.end, state.oracle.now());
    Ok(Json(plan))
}

/// Assesses the window, stations and metrics of `key` now.
async fn assess(state: &AppState, key: PlanKey) -> Result<WindowCompatibility, AppError> {
    let PlanKey {
        start,
        end,
        stations,
        metrics,
    } = key;
    let now = state.oracle.now();
    let issued_end = now.min(start.saturating_sub(Duration::nanoseconds(1)));
    let forecast_request = ForecastRequest {
        start: Some(start),
        end: Some(end),
        generated_start: Some(issued_end.saturating_sub(MAX_WINDOW)),
        generated_end: Some(issued_end),
        station_ids: stations.join(","),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let (forecast, capabilities) = tokio::join!(
        state
            .weather_db
            .forecast_assessment(&forecast_request, stations.clone()),
        state
            .weather_db
            .precipitation_station_capabilities(stations.clone()),
    );
    let forecast = forecast?;
    let (capabilities, warning) = match capabilities {
        Ok(stations) => (stations, None),
        Err(error) => {
            log::warn!("window planning: precipitation capability lookup failed: {error:#}");
            (vec![], Some("Recent fixed-hour precipitation capability could not be checked; station support remains unknown".into()))
        }
    };
    Ok(WindowCompatibility {
        source: "noaa_weather".into(),
        forecasts: stations.iter().flat_map(|station| metrics.iter()
            .map(|metric| metric_compatibility(station, metric, &forecast))).collect(),
        observations: observation_status(end, now),
        stations: stations.iter().map(|station| StationCapability {
            station_id: station.clone(),
            fixed_hour_precipitation_source: if capabilities.contains(station) {
                PrecipitationCapability::RecentlyObserved
            } else { PrecipitationCapability::Unknown },
        }).collect(),
        requested_window: RequestedWindow { start, end, station_ids: stations, metrics },
        evaluated_at: now,
        settlement_ready: false,
        precipitation_capability_warning: warning,
        notice: "Forecast compatibility is provisional. Native boundaries do not guarantee future measurements. Settlement must verify the announced window, station coverage, reporting cadence, and every enabled metric again.".into(),
    })
}
