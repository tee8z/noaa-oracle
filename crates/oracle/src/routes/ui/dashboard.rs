use std::sync::Arc;

use axum::{extract::State, http::HeaderMap, response::Html};

use crate::{
    db::EventStatus,
    templates::{
        dashboard_page,
        pages::dashboard::{dashboard_content, DashboardData},
        EventStats, WeatherDisplay,
    },
    AppState, ObservationRequest, TemperatureUnit,
};

/// Handler for the dashboard page (GET /)
/// Returns full page for normal requests, content only for HTMX requests
pub async fn dashboard_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Html<String> {
    let data = build_dashboard_data(&state).await;

    // Check if this is an HTMX request
    if headers.contains_key("hx-request") {
        // Return only the content for HTMX partial updates
        Html(dashboard_content(&data).into_string())
    } else {
        // Return full page for normal browser requests
        Html(dashboard_page(&state.remote_url, &data).into_string())
    }
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

    // Get weather data - either for active event stations or from all available data
    let weather = if active_stations.is_empty() {
        // No active events - show weather from latest available observation data
        get_latest_weather(state).await
    } else {
        // Show weather for active event stations
        get_weather_for_stations(state, &active_stations).await
    };

    // Get all available stations for the dropdown (from the weather data we already have)
    let all_stations: Vec<(String, String)> = weather
        .iter()
        .map(|w| (w.station_id.clone(), w.station_name.clone()))
        .collect();

    DashboardData {
        pubkey,
        npub,
        stats,
        weather,
        all_stations,
    }
}

/// Top 100 major US airport station IDs to show by default
/// Covers all 50 states and major population centers
const DEFAULT_MAJOR_AIRPORTS: &[&str] = &[
    // Top 30 busiest US airports
    "KATL", // Atlanta
    "KLAX", // Los Angeles
    "KORD", // Chicago O'Hare
    "KDFW", // Dallas/Fort Worth
    "KDEN", // Denver
    "KJFK", // New York JFK
    "KSFO", // San Francisco
    "KSEA", // Seattle
    "KLAS", // Las Vegas
    "KMCO", // Orlando
    "KEWR", // Newark
    "KMIA", // Miami
    "KPHX", // Phoenix
    "KIAH", // Houston Intercontinental
    "KBOS", // Boston
    "KMSP", // Minneapolis
    "KFLL", // Fort Lauderdale
    "KDTW", // Detroit
    "KPHL", // Philadelphia
    "KLGA", // New York LaGuardia
    "KBWI", // Baltimore
    "KSLC", // Salt Lake City
    "KDCA", // Washington Reagan
    "KSAN", // San Diego
    "KTPA", // Tampa
    "KPDX", // Portland OR
    "KSTL", // St. Louis
    "KHNL", // Honolulu
    "KBNA", // Nashville
    "KAUS", // Austin
    // Additional major airports (31-60)
    "KMCI", // Kansas City
    "KRDU", // Raleigh-Durham
    "KMKE", // Milwaukee
    "KSMF", // Sacramento
    "KCLT", // Charlotte
    "KPIT", // Pittsburgh
    "KSAT", // San Antonio
    "KOAK", // Oakland
    "KCLE", // Cleveland
    "KSJC", // San Jose
    "KIND", // Indianapolis
    "KCVG", // Cincinnati
    "KCMH", // Columbus OH
    "KJAN", // Jackson MS
    "KRSW", // Fort Myers
    "KABQ", // Albuquerque
    "KANC", // Anchorage
    "KOMA", // Omaha
    "KBUF", // Buffalo
    "KPBI", // West Palm Beach
    // Additional airports for state coverage (61-100)
    "KBDL", // Hartford CT
    "KPVD", // Providence RI
    "KBTV", // Burlington VT
    "KPWM", // Portland ME
    "KMHT", // Manchester NH
    "KBOI", // Boise ID
    "KBIL", // Billings MT
    "KFSD", // Sioux Falls SD
    "KFAR", // Fargo ND
    "KGEG", // Spokane WA
    "KICT", // Wichita KS
    "KLIT", // Little Rock AR
    "KLEX", // Lexington KY
    "KBHM", // Birmingham AL
    "KMEM", // Memphis TN
    "KJAX", // Jacksonville FL
    "KCHS", // Charleston SC
    "KRIC", // Richmond VA
    "KORF", // Norfolk VA
    "KCRW", // Charleston WV
    "KPNS", // Pensacola FL
    "KMOB", // Mobile AL
    "KSHV", // Shreveport LA
    "KMSY", // New Orleans
    "KTUL", // Tulsa OK
    "KELP", // El Paso TX
    "KTUS", // Tucson AZ
    "KCOS", // Colorado Springs
    "KGRR", // Grand Rapids MI
    "KDSM", // Des Moines IA
    "KMSN", // Madison WI
    "KDLH", // Duluth MN
    "KBZN", // Bozeman MT
    "KGJT", // Grand Junction CO
    "KRAP", // Rapid City SD
    "KFCA", // Kalispell MT
    "KCYS", // Cheyenne WY
    "KJAR", // Casper WY (KCPR)
    "KSGF", // Springfield MO
    "KFSM", // Fort Smith AR
];

/// Get weather from the latest available observation files (when no active events)
async fn get_latest_weather(state: &Arc<AppState>) -> Vec<WeatherDisplay> {
    // Query all available observation files to show current weather data
    // Using None for start/end finds all available data
    let req = ObservationRequest {
        start: None,
        end: None,
        station_ids: String::new(), // Empty = no filter, get all stations
        temperature_unit: TemperatureUnit::Fahrenheit,
    };

    let observations = state
        .weather_db
        .observation_data(&req, vec![]) // Empty vec = no station filter
        .await
        .unwrap_or_default();

    // Get station names for lookup
    let all_stations = state.weather_db.stations().await.unwrap_or_default();

    // First, try to get data for major airports
    let mut weather_data: Vec<WeatherDisplay> = Vec::new();

    for &airport_id in DEFAULT_MAJOR_AIRPORTS {
        if let Some(obs) = observations.iter().find(|o| o.station_id == airport_id) {
            let station = all_stations.iter().find(|s| s.station_id == airport_id);
            let station_name = station.map(|s| s.station_name.clone()).unwrap_or_default();

            weather_data.push(WeatherDisplay {
                station_id: obs.station_id.clone(),
                station_name,
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

    // If we found major airports, return those
    if !weather_data.is_empty() {
        return weather_data;
    }

    // Fallback: if no major airports found, return first 20 stations alphabetically
    let mut weather_data: Vec<WeatherDisplay> = observations
        .into_iter()
        .map(|obs| {
            let station = all_stations.iter().find(|s| s.station_id == obs.station_id);

            WeatherDisplay {
                station_id: obs.station_id.clone(),
                station_name: station.map(|s| s.station_name.clone()).unwrap_or_default(),
                state: station.map(|s| s.state.clone()).unwrap_or_default(),
                iata_id: station.map(|s| s.iata_id.clone()).unwrap_or_default(),
                elevation_m: station.and_then(|s| s.elevation_m),
                temp_high: Some(obs.temp_high),
                temp_low: Some(obs.temp_low),
                wind_speed: Some(obs.wind_speed),
                last_updated: obs.end_time,
            }
        })
        .collect();

    weather_data.sort_by(|a, b| a.station_id.cmp(&b.station_id));
    weather_data.truncate(20);
    weather_data
}

async fn get_weather_for_stations(
    state: &Arc<AppState>,
    station_ids: &[String],
) -> Vec<WeatherDisplay> {
    if station_ids.is_empty() {
        return Vec::new();
    }

    let mut weather_data = Vec::new();

    // Query all available observation files for these stations
    let req = ObservationRequest {
        start: None,
        end: None,
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
