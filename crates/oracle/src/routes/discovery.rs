//! Eligible stations and their display forecasts in one bounded query.
//! Discovery evidence does not authorize settlement or promise future observations.

use axum::{
    Json,
    extract::{Query, State},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};
use time::{Duration, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use utoipa::{IntoParams, ToSchema};

use crate::{
    AppError, AppState,
    heavy::Turns,
    routes::{Audience, ForecastRequest, TemperatureUnit},
    weather_data::{DEFAULT_DAYS, EligibleStation, Forecast, PRECOMPUTED_DAYS, WeatherData},
};

const MAX_STATIONS: usize = 5000;

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryRequest {
    /// Full UTC observation days to judge, 1 to 3; defaults to 3.
    pub days: Option<u32>,
    #[serde(with = "time::serde::rfc3339")]
    pub start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub end: OffsetDateTime,
}

impl DiscoveryRequest {
    /// The history days and eligibility window hours, for at most
    /// `max_days` of history (see [`Audience::max_eligible_days`]).
    fn checked(&self, now: OffsetDateTime, max_days: u32) -> Result<(u32, u32), AppError> {
        let days = self.days.unwrap_or(DEFAULT_DAYS);
        let duration = self.end - self.start;
        if !(1..=max_days).contains(&days)
            || self.start < now
            || self.start > now + Duration::days(7)
            || duration < Duration::HOUR
            || duration > Duration::hours(48)
            || duration.whole_seconds() % 3600 != 0
            || duration.subsec_nanoseconds() != 0
        {
            return Err(AppError::InvalidRequest(format!(
                "Discovery needs 1–{max_days} history days and a future window of 1–48 whole \
                 hours starting within seven days"
            )));
        }
        Ok((days, duration.whole_hours().min(24) as u32))
    }
}

/// One discovery question: the coordinator asks the same few over and over,
/// so answers are kept by it (see [`crate::heavy::Kept`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiscoveryKey {
    days: u32,
    /// In UTC, so the same instants written with other offsets share an
    /// answer.
    start: OffsetDateTime,
    end: OffsetDateTime,
}

impl DiscoveryKey {
    fn new(days: u32, request: &DiscoveryRequest) -> Self {
        Self {
            days,
            start: request.start.to_offset(UtcOffset::UTC),
            end: request.end.to_offset(UtcOffset::UTC),
        }
    }

    /// Retained bytes of a kept answer, roughly: a few hundred bytes for
    /// each station and forecast row.
    pub(crate) fn estimated_bytes(&self, answer: &Arc<DiscoveryForecasts>) -> usize {
        std::mem::size_of::<Self>()
            + std::mem::size_of::<DiscoveryForecasts>()
            + answer.stations.capacity() * (std::mem::size_of::<EligibleStation>() + 128)
            + answer.forecasts.capacity() * (std::mem::size_of::<Forecast>() + 128)
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
    let mut stations: Vec<_> = eligible
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
    let metrics = if request.end - request.start >= Duration::DAY {
        vec!["temp_high", "temp_low", "wind_speed"]
    } else if request.start.to_offset(time::UtcOffset::UTC).hour() >= 12 {
        vec!["temp_high", "wind_speed"]
    } else {
        vec!["temp_low", "wind_speed"]
    }
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let ids = weather
        .forecast_candidates(&forecast_request, ids, &metrics)
        .await?;
    let available: HashSet<_> = ids.iter().cloned().collect();
    stations.retain(|station| available.contains(&station.station_id));
    if ids.is_empty() {
        return Ok(DiscoveryForecasts {
            stations,
            forecasts: vec![],
        });
    }
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

/// Answers are kept per history and window until new forecasts or a new
/// judgment of eligibility arrive, at most 10 minutes; a stale answer is
/// served while it is built again. Building one is heavy work: it waits
/// for a turn and the request gets 503 with `Retry-After` when none comes.
#[utoipa::path(
    get,
    path = "/stations/eligible/forecasts",
    params(DiscoveryRequest),
    responses(
        (status = OK, description = "Eligible stations with forecast extent through the requested window and quality-filtered display forecasts", body = DiscoveryForecasts),
        (status = BAD_REQUEST, description = "Invalid discovery window or history"),
        (status = SERVICE_UNAVAILABLE, description = "Busy; retry after the Retry-After delay"),
        (status = INTERNAL_SERVER_ERROR, description = "Eligibility or forecasts unavailable")
    )
)]
pub async fn eligible_forecasts(
    audience: Audience,
    State(state): State<Arc<AppState>>,
    Query(request): Query<DiscoveryRequest>,
) -> Result<Response, AppError> {
    let (days, hours) = request.checked(OffsetDateTime::now_utc(), audience.max_eligible_days())?;
    let key = DiscoveryKey::new(days, &request);
    let generation = state.discovery_generation();
    // Weeks of history run alone, as eligible lists do.
    let turns = if days > PRECOMPUTED_DAYS {
        Turns::Every
    } else {
        Turns::One
    };
    let builder = state.clone();
    let answer = state
        .discoveries()
        .get(state.heavy(), turns, key, generation, move || async move {
            // The build holds a heavy turn already.
            let stations = builder.eligible_stations_on_turn(days, hours).await?;
            let found = collect(builder.weather_db.as_ref(), &stations, &request).await?;
            Ok::<_, AppError>(Arc::new(found))
        })
        .await
        .map_err(|error| state.turned_away(error))?;
    Ok(Json(answer.as_ref()).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn discovery_windows_are_future_and_bounded_before_reading_weather() {
        let now = datetime!(2026-10-03 12:00 UTC);
        let operator = Audience::Operator.max_eligible_days();
        let mut request = DiscoveryRequest {
            days: None,
            start: now + Duration::DAY,
            end: now + Duration::days(2),
        };
        assert_eq!(request.checked(now, operator).unwrap(), (3, 24));
        request.end = request.start + Duration::hours(12);
        assert_eq!(request.checked(now, operator).unwrap(), (3, 12));
        request.end = request.start + Duration::hours(48);
        assert_eq!(request.checked(now, operator).unwrap(), (3, 24));
        for hours in [0, 49] {
            request.end = request.start + Duration::hours(hours);
            assert!(request.checked(now, operator).is_err());
        }
        request.end = request.start + Duration::hours(24) + Duration::SECOND;
        assert!(request.checked(now, operator).is_err());
        request.end = request.start + Duration::DAY;
        for days in [0, 32] {
            request.days = Some(days);
            assert!(request.checked(now, operator).is_err());
        }
        request.days = Some(31);
        assert_eq!(request.checked(now, operator).unwrap(), (31, 24));
        request.days = Some(3);
        request.start = now - Duration::SECOND;
        assert!(request.checked(now, operator).is_err());
        request.start = now + Duration::days(8);
        request.end = request.start + Duration::DAY;
        assert!(request.checked(now, operator).is_err());
    }

    #[test]
    fn public_discovery_judges_only_the_days_read_ahead() {
        let now = datetime!(2026-10-03 12:00 UTC);
        let public = Audience::Public.max_eligible_days();
        let mut request = DiscoveryRequest {
            days: Some(3),
            start: now + Duration::DAY,
            end: now + Duration::days(2),
        };
        assert_eq!(request.checked(now, public).unwrap(), (3, 24));
        for days in [4, 7, 30] {
            request.days = Some(days);
            let error = request.checked(now, public).unwrap_err().to_string();
            assert!(error.contains("1–3 history days"), "{error}");
        }
    }

    #[test]
    fn the_same_instants_with_other_offsets_share_an_answer() {
        let start = datetime!(2026-10-04 00:00 UTC);
        let request = |start: OffsetDateTime| DiscoveryRequest {
            days: Some(3),
            start,
            end: start + Duration::DAY,
        };
        let utc = DiscoveryKey::new(3, &request(start));
        let eastern = DiscoveryKey::new(3, &request(start.to_offset(time::macros::offset!(-4))));
        assert_eq!(utc, eastern);
        assert_eq!(utc.start.offset(), UtcOffset::UTC);
        assert_ne!(utc, DiscoveryKey::new(2, &request(start)));
    }
}
