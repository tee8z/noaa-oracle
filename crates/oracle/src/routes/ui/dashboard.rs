use std::sync::Arc;

use axum::{extract::State, response::Html};
use time::OffsetDateTime;

use crate::{
    db::EventStatus,
    templates::{dashboard_page, pages::dashboard::DashboardData, EventStats, WeatherDisplay},
    AppState, ObservationRequest, TemperatureUnit,
};

/// Handler for the dashboard page (GET /)
pub async fn dashboard_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let data = build_dashboard_data(&state).await;
    Html(dashboard_page(&state.remote_url, &data).into_string())
}

async fn build_dashboard_data(state: &Arc<AppState>) -> DashboardData {
    // Get oracle identity
    let pubkey = state.oracle.public_key();
    let npub = state.oracle.npub().unwrap_or_else(|_| "Error".to_string());

    // Get event statistics
    let events = state
        .oracle
        .list_events(crate::db::EventFilter::default())
        .await
        .unwrap_or_default();

    let mut stats = EventStats::default();
    let mut active_stations: Vec<String> = Vec::new();

    for event in &events {
        match event.status {
            EventStatus::Live => {
                stats.live_count += 1;
                // Collect stations from live events
                active_stations.extend(event.locations.clone());
            }
            EventStatus::Running => {
                stats.running_count += 1;
                // Collect stations from running events
                active_stations.extend(event.locations.clone());
            }
            EventStatus::Completed => stats.completed_count += 1,
            EventStatus::Signed => stats.signed_count += 1,
        }
    }

    // Deduplicate stations
    active_stations.sort();
    active_stations.dedup();

    // Get weather data for active stations
    let weather = get_weather_for_stations(state, &active_stations).await;

    // Get all available stations for the dropdown
    let all_stations = state
        .weather_db
        .stations()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.station_id, s.station_name))
        .collect();

    DashboardData {
        pubkey,
        npub,
        stats,
        weather,
        all_stations,
    }
}

async fn get_weather_for_stations(
    state: &Arc<AppState>,
    station_ids: &[String],
) -> Vec<WeatherDisplay> {
    if station_ids.is_empty() {
        return Vec::new();
    }

    let mut weather_data = Vec::new();

    // Get recent observations for each station
    let now = OffsetDateTime::now_utc();
    let start = now - time::Duration::hours(24);

    let req = ObservationRequest {
        start: Some(start),
        end: Some(now),
        station_ids: station_ids.join(","),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };

    let observations = state
        .weather_db
        .observation_data(&req, station_ids.to_vec())
        .await
        .unwrap_or_default();

    // Get all stations for name lookup
    let all_stations = state.weather_db.stations().await.unwrap_or_default();

    for station_id in station_ids {
        // Find the most recent observation for this station
        if let Some(obs) = observations.iter().find(|o| o.station_id == *station_id) {
            let station_name = all_stations
                .iter()
                .find(|s| s.station_id == *station_id)
                .map(|s| s.station_name.clone())
                .unwrap_or_default();

            weather_data.push(WeatherDisplay {
                station_id: station_id.clone(),
                station_name,
                temp_high: Some(obs.temp_high),
                temp_low: Some(obs.temp_low),
                wind_speed: Some(obs.wind_speed),
                last_updated: obs.end_time.clone(),
            });
        }
    }

    weather_data
}
