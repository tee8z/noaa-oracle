use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Html,
};
use serde::Deserialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    AppState,
    events::EventStatus,
    templates::{
        EventStats, WeatherDisplay, dashboard_page,
        pages::dashboard::{DashboardData, dashboard_content},
    },
};

#[derive(Debug, Deserialize, Default)]
pub struct DashboardQuery {
    /// Comma-separated station ids; defaults to available major airports.
    pub stations: Option<String>,
    /// Start time for observation data (RFC3339 format)
    pub start: Option<String>,
    /// End time for observation data (RFC3339 format)
    pub end: Option<String>,
}

/// Handler for the dashboard page (GET /)
/// Returns full page for normal requests, content only for HTMX requests
/// Optional `start` and `end` query params override the default UTC day so far.
pub async fn dashboard_handler(
    headers: HeaderMap,
    Query(query): Query<DashboardQuery>,
    State(state): State<Arc<AppState>>,
) -> Html<String> {
    // Parse optional time range from query params
    let start = query
        .start
        .as_ref()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok());
    let end = query
        .end
        .as_ref()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok());

    let station_ids = query.stations.as_deref().map(|stations| {
        stations
            .split(',')
            .map(str::trim)
            .filter(|station| !station.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    let data = build_dashboard_data(&state, station_ids.as_deref(), start, end).await;

    // Check if this is an HTMX request
    if headers.contains_key("hx-request") {
        // Return only the content for HTMX partial updates
        Html(dashboard_content(&data).into_string())
    } else {
        // Return full page for normal browser requests
        Html(dashboard_page(&state.remote_url, &data).into_string())
    }
}

async fn build_dashboard_data(
    state: &Arc<AppState>,
    station_ids: Option<&[String]>,
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
) -> DashboardData {
    // Get oracle identity
    let pubkey = state.oracle.public_key_base64();
    let npub = state.oracle.npub();

    // Get event statistics
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

    let weather = get_latest_weather(state, station_ids, start, end).await;
    let displayed_ids: Vec<String> = weather
        .iter()
        .map(|weather| weather.station_id.clone())
        .collect();
    let weather_refresh_path =
        super::weather::refresh_path(station_ids.unwrap_or(&displayed_ids), start, end);

    // The selector remains useful when the current selection has no reports.
    let available_stations = state.stations().await.unwrap_or_default();
    let all_stations: Vec<(String, String)> = available_stations
        .iter()
        .map(|station| (station.station_id.clone(), station.station_name.clone()))
        .collect();

    DashboardData {
        pubkey,
        npub,
        stats,
        weather,
        all_stations,
        weather_refresh_path,
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

/// Geographic region based on longitude, ordered West to East
fn get_region(longitude: f64) -> u8 {
    if longitude < -140.0 {
        0 // Alaska/Hawaii (far west)
    } else if longitude < -115.0 {
        1 // Pacific (West Coast)
    } else if longitude < -100.0 {
        2 // Mountain
    } else if longitude < -85.0 {
        3 // Central (Midwest/South Central)
    } else {
        4 // Eastern (East Coast + Southeast)
    }
}

/// Get weather from the latest available observation files
async fn get_latest_weather(
    state: &Arc<AppState>,
    station_ids: Option<&[String]>,
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
) -> Vec<WeatherDisplay> {
    let mut weather_data =
        super::weather::load_weather(state, station_ids.unwrap_or_default(), start, end).await;
    if station_ids.is_some() {
        weather_data.sort_by(|a, b| a.station_id.cmp(&b.station_id));
        return weather_data;
    }
    if weather_data
        .iter()
        .any(|weather| DEFAULT_MAJOR_AIRPORTS.contains(&weather.station_id.as_str()))
    {
        weather_data
            .retain(|weather| DEFAULT_MAJOR_AIRPORTS.contains(&weather.station_id.as_str()));
        weather_data.sort_by(|a, b| {
            get_region(b.longitude)
                .cmp(&get_region(a.longitude))
                .then_with(|| b.latitude.total_cmp(&a.latitude))
        });
    } else {
        weather_data.sort_by(|a, b| a.station_id.cmp(&b.station_id));
        weather_data.truncate(20);
    }
    weather_data
}
