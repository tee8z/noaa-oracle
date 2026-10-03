//! Eligible stations and their display forecasts in one bounded query.
//! Discovery evidence does not authorize settlement or promise future observations.

use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use utoipa::{IntoParams, ToSchema};

use crate::{
    AppError, AppState,
    routes::{ForecastRequest, TemperatureUnit},
    weather_data::{EligibleStation, Forecast, WeatherData},
};

const MAX_STATIONS: usize = 5000;

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryRequest {
    /// Full UTC observation days to judge, 1 to 31; defaults to 3.
    pub days: Option<u32>,
    #[serde(with = "time::serde::rfc3339")]
    pub start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end: OffsetDateTime,
}

impl DiscoveryRequest {
    fn checked(&self, now: OffsetDateTime) -> Result<(u32, u32), AppError> {
        let days = self.days.unwrap_or(3);
        let duration = self.end - self.start;
        if !(1..=31).contains(&days)
            || self.start < now
            || self.start > now + Duration::days(7)
            || duration < Duration::HOUR
            || duration > Duration::hours(48)
            || duration.whole_seconds() % 3600 != 0
            || duration.subsec_nanoseconds() != 0
        {
            return Err(AppError::InvalidRequest("Discovery needs 1–31 history days and a future window of 1–48 whole hours starting within seven days".into()));
        }
        Ok((days, duration.whole_hours().min(24) as u32))
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DiscoveryForecasts {
    pub stations: Vec<EligibleStation>,
    pub forecasts: Vec<Forecast>,
}

/// Keep the same eligibility and forecast-quality paths as the individual APIs.
/// Empty eligibility must not become an unfiltered forecast query.
async fn collect(
    weather: &dyn WeatherData,
    eligible: &[EligibleStation],
    request: &DiscoveryRequest,
) -> Result<DiscoveryForecasts, AppError> {
    if eligible.len() > MAX_STATIONS {
        return Err(AppError::InvalidRequest(
            "Too many eligible stations for discovery".into(),
        ));
    }
    let stations: Vec<_> = eligible
        .iter()
        .filter(|station| {
            OffsetDateTime::parse(&station.forecast_through, &Rfc3339)
                .is_ok_and(|through| through >= request.end)
        })
        .cloned()
        .collect();
    if stations.is_empty() {
        return Ok(DiscoveryForecasts {
            stations,
            forecasts: vec![],
        });
    }
    let ids: Vec<_> = stations
        .iter()
        .map(|station| station.station_id.clone())
        .collect();
    let forecast_request = ForecastRequest {
        start: Some(request.start),
        end: Some(request.end),
        generated_start: None,
        generated_end: None,
        station_ids: ids.join(","),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let mut forecasts = weather.forecasts_data(&forecast_request, ids).await?;
    let allowed: HashSet<_> = stations
        .iter()
        .map(|station| station.station_id.as_str())
        .collect();
    forecasts.retain(|forecast| allowed.contains(forecast.station_id.as_str()));
    Ok(DiscoveryForecasts {
        stations,
        forecasts,
    })
}

#[utoipa::path(
    get,
    path = "/stations/eligible/forecasts",
    params(DiscoveryRequest),
    responses(
        (status = OK, description = "Eligible stations with forecast extent through the requested window and quality-filtered display forecasts", body = DiscoveryForecasts),
        (status = BAD_REQUEST, description = "Invalid discovery window or history"),
        (status = SERVICE_UNAVAILABLE, description = "Weather query capacity unavailable"),
        (status = INTERNAL_SERVER_ERROR, description = "Eligibility or forecasts unavailable")
    )
)]
pub async fn eligible_forecasts(
    State(state): State<Arc<AppState>>,
    Query(request): Query<DiscoveryRequest>,
) -> Result<Json<DiscoveryForecasts>, AppError> {
    let (days, hours) = request.checked(OffsetDateTime::now_utc())?;
    let stations = state.eligible_stations(days, hours).await?;
    Ok(Json(
        collect(state.weather_db.as_ref(), &stations, &request).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn discovery_windows_are_future_and_bounded_before_reading_weather() {
        let now = datetime!(2026-10-03 12:00 UTC);
        let mut request = DiscoveryRequest {
            days: None,
            start: now + Duration::DAY,
            end: now + Duration::days(2),
        };
        assert_eq!(request.checked(now).unwrap(), (3, 24));
        request.end = request.start + Duration::hours(12);
        assert_eq!(request.checked(now).unwrap(), (3, 12));
        request.end = request.start + Duration::hours(48);
        assert_eq!(request.checked(now).unwrap(), (3, 24));
        for hours in [0, 49] {
            request.end = request.start + Duration::hours(hours);
            assert!(request.checked(now).is_err());
        }
        request.end = request.start + Duration::hours(24) + Duration::SECOND;
        assert!(request.checked(now).is_err());
        request.end = request.start + Duration::DAY;
        for days in [0, 32] {
            request.days = Some(days);
            assert!(request.checked(now).is_err());
        }
        request.days = Some(3);
        request.start = now - Duration::SECOND;
        assert!(request.checked(now).is_err());
        request.start = now + Duration::days(8);
        request.end = request.start + Duration::DAY;
        assert!(request.checked(now).is_err());
    }
}
