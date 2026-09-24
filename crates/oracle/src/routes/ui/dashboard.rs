use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::Response,
};
use serde::Deserialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::htmx::{Render, page_or_fragment};
use crate::{
    AppState,
    events::EventStatus,
    templates::{
        fragments::{EventStats, WeatherContext, WeatherDisplay, WeatherView, weather_section},
        pages::dashboard::{DashboardData, dashboard_fragment, dashboard_page},
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
    /// `map` or `list`; defaults to the reader's last choice.
    pub view: Option<String>,
    /// Station search in the list.
    pub q: Option<String>,
}

/// The cookie that remembers Map or List between visits.
pub(super) const VIEW_COOKIE: &str = "weather_view";

/// The view asked for, else the one remembered in the cookie, else the map.
pub(super) fn chosen_view(asked: Option<&str>, headers: &HeaderMap) -> WeatherView {
    WeatherView::parse(asked)
        .or_else(|| {
            headers
                .get_all(header::COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|cookies| cookies.split(';'))
                .filter_map(|cookie| cookie.trim().split_once('='))
                .find(|(name, _)| *name == VIEW_COOKIE)
                .and_then(|(_, value)| WeatherView::parse(Some(value)))
        })
        .unwrap_or_default()
}

/// Remembers the view for a year.
pub(super) fn remember_view(response: &mut Response, view: WeatherView) {
    let cookie = format!(
        "{VIEW_COOKIE}={}; Path=/; Max-Age=31536000; SameSite=Lax",
        view.as_str()
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

/// Handler for the dashboard page (GET /)
/// Returns full page for normal requests, content only for HTMX requests
/// Optional `start` and `end` query params override the default UTC day so far.
pub async fn dashboard_handler(
    headers: HeaderMap,
    Query(query): Query<DashboardQuery>,
    State(state): State<Arc<AppState>>,
) -> Response {
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
    let days = super::local_day::reader_day(&headers, OffsetDateTime::now_utc());
    let (data, selection_path) =
        build_dashboard_data(&state, station_ids.as_deref(), start, end, days).await;
    let stations = state.stations().await.unwrap_or_default();
    let view = chosen_view(query.view.as_deref(), &headers);
    let context = WeatherContext {
        view,
        query: query.q.as_deref().unwrap_or_default(),
        selection_path: &selection_path,
        stations: &stations,
        now: OffsetDateTime::now_utc(),
    };

    let mut response = page_or_fragment(match super::htmx::render(&headers) {
        Render::Page => dashboard_page(&data, &context).into_string(),
        Render::Content => dashboard_fragment(&data, &context).into_string(),
        Render::Part(_) => weather_section(&data.weather, &context).into_string(),
    });
    if query.view.is_some() {
        remember_view(&mut response, view);
    }
    response
}

/// The dashboard's data and the `/fragments/weather` path that refreshes it.
async fn build_dashboard_data(
    state: &Arc<AppState>,
    station_ids: Option<&[String]>,
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
    days: super::local_day::ReaderDay,
) -> (DashboardData, String) {
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

    let (weather, default_airports) =
        get_latest_weather(state, station_ids, start, end, days).await;
    // The default airports refresh without naming them, which keeps the
    // address bar short; any other selection names its stations.
    let displayed_ids: Vec<String> = if default_airports {
        vec![]
    } else {
        weather
            .iter()
            .map(|weather| weather.station_id.clone())
            .collect()
    };
    let selection_path =
        super::weather::refresh_path(station_ids.unwrap_or(&displayed_ids), start, end);

    (
        DashboardData {
            pubkey,
            npub,
            stats,
            weather,
        },
        selection_path,
    )
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

/// Get weather from the latest available observation files. Also says
/// whether these are the default airports, which need not be named.
async fn get_latest_weather(
    state: &Arc<AppState>,
    station_ids: Option<&[String]>,
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
    days: super::local_day::ReaderDay,
) -> (Vec<WeatherDisplay>, bool) {
    if let Some(station_ids) = station_ids {
        let weather_data = super::weather::load_weather(state, station_ids, start, end, days).await;
        return (weather_data, false);
    }
    // Query only the default airports: a forecast query over every station
    // exceeds the per-query memory limit, which left the forecast column empty.
    let airports: Vec<String> = DEFAULT_MAJOR_AIRPORTS
        .iter()
        .map(|station| station.to_string())
        .collect();
    let weather_data = super::weather::load_weather(state, &airports, start, end, days).await;
    if !weather_data.is_empty() {
        return (weather_data, true);
    }
    // Data without any default airport: show the first stations reporting.
    let mut weather_data = super::weather::load_weather(state, &[], start, end, days).await;
    weather_data.sort_by(|a, b| a.station_id.cmp(&b.station_id));
    weather_data.truncate(20);
    (weather_data, false)
}
