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
        fragments::{event_stats, forecast_detail, oracle_info, weather_table_body},
        EventStats, ForecastComparison, ForecastDisplay, WeatherDisplay,
    },
    AppState, ForecastRequest, ObservationRequest, TemperatureUnit,
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

    // Return last 3 days of data - frontend will filter by user's local calendar day
    // This ensures we have enough data for any timezone
    let start = now - time::Duration::days(3);

    // Current time for "updated_at" field
    let updated_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

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
                observed_start: obs.start_time.clone(),
                observed_end: obs.end_time.clone(),
                updated_at: updated_at.clone(),
                latitude: station.map(|s| s.latitude).unwrap_or(0.0),
                longitude: station.map(|s| s.longitude).unwrap_or(0.0),
            });
        }
    }

    weather_data
}

/// Handler for forecast detail fragment (GET /fragments/forecast/:station_id)
pub async fn forecast_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(station_id): axum::extract::Path<String>,
) -> Html<String> {
    let now = OffsetDateTime::now_utc();

    // Fetch upcoming forecasts (today + 7 days)
    let future_end = now + time::Duration::days(7);
    let future_req = ForecastRequest {
        start: Some(now),
        end: Some(future_end),
        generated_start: None,
        generated_end: None,
        station_ids: station_id.clone(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };

    let forecasts = state
        .weather_db
        .forecasts_data(&future_req, vec![station_id.clone()])
        .await
        .unwrap_or_default();

    let mut forecast_displays: Vec<ForecastDisplay> = forecasts
        .into_iter()
        .map(|f| ForecastDisplay {
            date: f.date,
            temp_high: f.temp_high,
            temp_low: f.temp_low,
            wind_speed: f.wind_speed,
            precip_chance: f.precip_chance,
        })
        .collect();

    // Sort by date chronologically
    forecast_displays.sort_by(|a, b| a.date.cmp(&b.date));

    // Fetch past forecasts for comparison - use forecasts generated before the period started
    let past_start = now - time::Duration::days(3);
    let past_req = ForecastRequest {
        start: Some(past_start),
        end: Some(now),
        generated_start: Some(past_start - time::Duration::days(1)),
        generated_end: Some(past_start),
        station_ids: station_id.clone(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };

    let past_forecasts = state
        .weather_db
        .forecasts_data(&past_req, vec![station_id.clone()])
        .await
        .unwrap_or_default();

    // Fetch daily observations for the same period
    let obs_req = ObservationRequest {
        start: Some(past_start),
        end: Some(now),
        station_ids: station_id.clone(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };

    let daily_obs = state
        .weather_db
        .daily_observations(&obs_req, vec![station_id.clone()])
        .await
        .unwrap_or_default();

    // Build comparison data by matching forecast dates to observation dates
    let mut comparisons: Vec<ForecastComparison> = past_forecasts
        .into_iter()
        .map(|f| {
            // Find matching observation for this date
            let obs = daily_obs.iter().find(|o| o.date == f.date);
            ForecastComparison {
                date: f.date,
                forecast_high: f.temp_high,
                forecast_low: f.temp_low,
                actual_high: obs.map(|o| o.temp_high),
                actual_low: obs.map(|o| o.temp_low),
            }
        })
        .collect();

    // Sort comparisons by date (most recent first)
    comparisons.sort_by(|a, b| b.date.cmp(&a.date));

    Html(forecast_detail(&station_id, &comparisons, &forecast_displays).into_string())
}
