use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::Html,
};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::{
    db::EventStatus,
    templates::{
        fragments::{event_stats, oracle_info, weather_table_body},
        EventStats, WeatherDisplay,
    },
    AppState, ObservationRequest, TemperatureUnit,
};

#[derive(Debug, Deserialize)]
pub struct WeatherQuery {
    pub stations: Option<String>,
    pub add_station: Option<String>,
}

/// Handler for oracle info fragment (GET /fragments/oracle-info)
pub async fn oracle_info_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let pubkey = state.oracle.public_key();
    let npub = state.oracle.npub().unwrap_or_else(|_| "Error".to_string());
    Html(oracle_info(&pubkey, &npub).into_string())
}

/// Handler for event stats fragment (GET /fragments/event-stats)
pub async fn event_stats_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let events = state
        .oracle
        .list_events(crate::db::EventFilter::default())
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
    State(state): State<Arc<AppState>>,
    Query(query): Query<WeatherQuery>,
) -> Html<String> {
    // Get stations from query or from active events
    let mut station_ids: Vec<String> = if let Some(stations) = &query.stations {
        stations.split(',').map(|s| s.trim().to_string()).collect()
    } else {
        // Default to stations from active events
        let events = state
            .oracle
            .list_events(crate::db::EventFilter::default())
            .await
            .unwrap_or_default();

        let mut active_stations: Vec<String> = events
            .iter()
            .filter(|e| matches!(e.status, EventStatus::Live | EventStatus::Running))
            .flat_map(|e| e.locations.clone())
            .collect();

        active_stations.sort();
        active_stations.dedup();
        active_stations
    };

    // Add station if requested
    if let Some(add_station) = &query.add_station {
        if !station_ids.contains(add_station) {
            station_ids.push(add_station.clone());
        }
    }

    let weather = get_weather_for_stations(&state, &station_ids).await;
    Html(weather_table_body(&weather).into_string())
}

async fn get_weather_for_stations(
    state: &Arc<AppState>,
    station_ids: &[String],
) -> Vec<WeatherDisplay> {
    if station_ids.is_empty() {
        return Vec::new();
    }

    let mut weather_data = Vec::new();

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
        if let Some(obs) = observations.iter().find(|o| o.station_id == *station_id) {
            let station = all_stations.iter().find(|s| s.station_id == *station_id);

            weather_data.push(WeatherDisplay {
                station_id: station_id.clone(),
                station_name: station.map(|s| s.station_name.clone()).unwrap_or_default(),
                state: station.map(|s| s.state.clone()).unwrap_or_default(),
                iata_id: station.map(|s| s.iata_id.clone()).unwrap_or_default(),
                elevation_m: station.and_then(|s| s.elevation_m),
                temp_high: Some(obs.temp_high),
                temp_low: Some(obs.temp_low),
                wind_speed: Some(obs.wind_speed),
                last_updated: obs.end_time.clone(),
            });
        }
    }

    weather_data
}
