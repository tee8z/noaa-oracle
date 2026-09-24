use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use futures::stream::{self, StreamExt};
use log::info;
use serde::Deserialize;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

use crate::{
    AppState, ForecastRequest, ObservationRequest, TemperatureUnit,
    events::EventStatus,
    templates::{
        EventStats, ForecastComparison, ForecastDisplay,
        fragments::{event_stats, forecast_detail, oracle_info, weather_table_body_with_refresh},
    },
    weather_data::validate_station_id,
};

/// Top 100 major US airport station IDs to show by default
const DEFAULT_MAJOR_AIRPORTS: &[&str] = &[
    "KATL", "KLAX", "KORD", "KDFW", "KDEN", "KJFK", "KSFO", "KSEA", "KLAS", "KMCO", "KEWR", "KMIA",
    "KPHX", "KIAH", "KBOS", "KMSP", "KFLL", "KDTW", "KPHL", "KLGA", "KBWI", "KSLC", "KDCA", "KSAN",
    "KTPA", "KPDX", "KSTL", "KHNL", "KBNA", "KAUS", "KMCI", "KRDU", "KMKE", "KSMF", "KCLT", "KPIT",
    "KSAT", "KOAK", "KCLE", "KSJC", "KIND", "KCVG", "KCMH", "KJAN", "KRSW", "KABQ", "KANC", "KOMA",
    "KBUF", "KPBI", "KBDL", "KPVD", "KBTV", "KPWM", "KMHT", "KBOI", "KBIL", "KFSD", "KFAR", "KGEG",
    "KICT", "KLIT", "KLEX", "KBHM", "KMEM", "KJAX", "KCHS", "KRIC", "KORF", "KCRW", "KPNS", "KMOB",
    "KSHV", "KMSY", "KTUL", "KELP", "KTUS", "KCOS", "KGRR", "KDSM", "KMSN", "KDLH", "KBZN", "KGJT",
    "KRAP", "KFCA", "KCYS", "KJAR", "KSGF", "KFSM",
];

#[derive(Debug, Deserialize)]
pub struct WeatherQuery {
    pub stations: Option<String>,
    pub add_station: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
}

/// Handler for oracle info fragment (GET /fragments/oracle-info)
pub async fn oracle_info_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let pubkey = state.oracle.public_key_base64();
    let npub = state.oracle.npub();
    Html(oracle_info(&pubkey, &npub).into_string())
}

/// Handler for event stats fragment (GET /fragments/event-stats)
pub async fn event_stats_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let events = state
        .oracle
        .list_events(crate::events::EventFilter::default())
        .await
        .unwrap_or_default();

    let mut stats = EventStats::default();
    for event in &events {
        match event.status {
            EventStatus::Live => stats.live_count += 1,
            EventStatus::Running => stats.running_count += 1,
            EventStatus::Completed => stats.completed_count += 1,
            EventStatus::Signed => stats.signed_count += 1,
        }
    }

    Html(event_stats(&stats).into_string())
}

/// Handler for weather table fragment (GET /fragments/weather)
pub async fn weather_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<WeatherQuery>,
) -> Html<String> {
    // Get stations from query or use default major airports
    let mut station_ids: Vec<String> = if let Some(stations) = &query.stations {
        stations.split(',').map(|s| s.trim().to_string()).collect()
    } else {
        // Default to major airports
        DEFAULT_MAJOR_AIRPORTS
            .iter()
            .map(|s| s.to_string())
            .collect()
    };

    // Add station if requested
    if let Some(add_station) = &query.add_station
        && !station_ids.contains(add_station)
    {
        station_ids.push(add_station.clone());
    }

    let start = query
        .start
        .as_deref()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok());
    let end = query
        .end
        .as_deref()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok());
    let days = super::local_day::reader_offset(&headers);
    let weather = super::weather::load_weather(&state, &station_ids, start, end, days).await;
    let refresh_path = super::weather::refresh_path(&station_ids, start, end);
    Html(weather_table_body_with_refresh(&weather, &refresh_path).into_string())
}

/// Handler for forecast detail fragment (GET /fragments/forecast/:station_id).
/// Days are the reader's (see [`super::local_day`]). Rejects invalid ids and
/// caches only stations present in the data, so arbitrary paths cannot grow
/// the cache.
pub async fn forecast_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Path(station_id): Path<String>,
) -> Response {
    if validate_station_id(&station_id).is_err() {
        return (StatusCode::BAD_REQUEST, "invalid station id").into_response();
    }
    let days = super::local_day::reader_offset(&headers);
    let key = forecast_cache_key(&station_id, days);
    if let Some(cached) = state.cached_forecast(&key) {
        return Html(cached).into_response();
    }
    let html = build_forecast_html(&state, &station_id, days).await;
    if state.is_known_station(&station_id).await {
        state.cache_forecast(key, html.clone());
    }
    Html(html).into_response()
}

/// Forecast fragments differ by the reader's calendar; UTC keeps the plain
/// station id the cache warmer uses.
fn forecast_cache_key(station_id: &str, days: UtcOffset) -> String {
    if days == UtcOffset::UTC {
        station_id.to_owned()
    } else {
        format!("{station_id}@{}", days.whole_minutes())
    }
}

/// Build the forecast detail HTML for a station (used by handler and cache warming)
pub async fn build_forecast_html(
    state: &Arc<AppState>,
    station_id: &str,
    days: UtcOffset,
) -> String {
    let now = OffsetDateTime::now_utc();

    // Forecasts and comparison observations use complete calendar days at
    // `days` from UTC.
    let today = super::local_day::start_of_today(now, days);
    let future_end = today + time::Duration::days(7);
    let future_req = ForecastRequest {
        start: Some(today),
        end: Some(future_end),
        generated_start: None,
        generated_end: None,
        station_ids: station_id.to_string(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };

    let forecasts = state
        .weather_db
        .local_forecasts(&future_req, vec![station_id.to_string()], days)
        .await
        .unwrap_or_default();

    let today_key = today.date().to_string();
    let mut forecast_displays: Vec<ForecastDisplay> = forecasts
        .into_iter()
        .filter(|forecast| {
            forecast
                .date
                .get(..10)
                .is_some_and(|date| date >= today_key.as_str())
        })
        .map(|f| ForecastDisplay {
            date: f.date,
            temp_high: f.temp_high,
            temp_low: f.temp_low,
            wind_speed: f.wind_speed,
            wind_direction: f.wind_direction,
            humidity_max: f.humidity_max,
            humidity_min: f.humidity_min,
            precip_chance: f.precip_chance,
            rain_amt: f.rain_amt,
            snow_amt: f.snow_amt,
        })
        .collect();

    // Sort by date chronologically
    forecast_displays.sort_by(|a, b| a.date.cmp(&b.date));

    // Fetch past forecasts and daily observations in parallel (last 7 days)
    let past_start = today - time::Duration::days(7);
    let past_req = ForecastRequest {
        start: Some(past_start),
        end: Some(today),
        generated_start: Some(past_start - time::Duration::days(1)),
        generated_end: Some(now),
        station_ids: station_id.to_string(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };

    let obs_req = ObservationRequest {
        start: Some(past_start),
        end: Some(today - time::Duration::nanoseconds(1)),
        station_ids: station_id.to_string(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };

    let (past_forecasts, daily_obs) = tokio::join!(
        state
            .weather_db
            .local_forecasts(&past_req, vec![station_id.to_string()], days),
        state
            .weather_db
            .local_daily_observations(&obs_req, vec![station_id.to_string()], days)
    );

    let past_forecasts = past_forecasts.unwrap_or_default();
    let daily_obs = daily_obs.unwrap_or_default();

    // Build comparison data by matching forecast dates to observation dates
    let past_key = past_start.date().to_string();
    let mut comparisons: Vec<ForecastComparison> = past_forecasts
        .into_iter()
        .filter(|forecast| {
            forecast
                .date
                .get(..10)
                .is_some_and(|date| date >= past_key.as_str() && date < today_key.as_str())
        })
        .map(|f| {
            let obs = daily_obs
                .iter()
                .find(|o| o.date.get(..10) == f.date.get(..10));
            ForecastComparison {
                date: f.date,
                forecast_high: f.temp_high,
                forecast_low: f.temp_low,
                forecast_wind: f.wind_speed,
                forecast_humidity_max: f.humidity_max,
                forecast_humidity_min: f.humidity_min,
                forecast_rain: f.rain_amt,
                forecast_snow: f.snow_amt,
                actual_high: obs.map(|o| o.temp_high),
                actual_low: obs.map(|o| o.temp_low),
                actual_wind: obs.and_then(|o| o.wind_speed),
                actual_humidity: obs.and_then(|o| o.humidity),
                actual_rain: obs.and_then(|o| o.rain_amt),
                actual_snow: obs.and_then(|o| o.snow_amt),
            }
        })
        .collect();

    // Sort comparisons by date (most recent first)
    comparisons.sort_by(|a, b| b.date.cmp(&a.date));

    forecast_detail(station_id, &comparisons, &forecast_displays).into_string()
}

/// Pre-warm the forecast cache for all default stations.
/// Called at startup and every 30 minutes by the background refresh task.
pub async fn warm_forecast_cache(state: &Arc<AppState>) {
    info!(
        "Warming forecast cache for {} stations...",
        DEFAULT_MAJOR_AIRPORTS.len()
    );

    let futs: Vec<_> = DEFAULT_MAJOR_AIRPORTS
        .iter()
        .map(|station_id| {
            let state = state.clone();
            let station_id = station_id.to_string();
            async move {
                let html = build_forecast_html(&state, &station_id, UtcOffset::UTC).await;
                state.cache_forecast(station_id, html);
            }
        })
        .collect();

    stream::iter(futs)
        .buffer_unordered(10)
        .collect::<Vec<()>>()
        .await;

    info!("Forecast cache warming complete.");
}
