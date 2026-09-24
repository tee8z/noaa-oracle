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
    calendar::Calendar,
    templates::{
        fragments::{WeatherContext, WeatherDisplay, WeatherView, weather_section},
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
        .or_else(|| WeatherView::parse(super::local_day::cookie(headers, VIEW_COOKIE)))
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
/// Optional `start` and `end` query params override the reader's day so far.
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

    let station_ids = super::weather::requested_stations(query.stations.as_deref());
    let calendar = super::local_day::reader_calendar(&headers);
    let (data, selection_path) =
        build_dashboard_data(&state, station_ids.as_deref(), start, end, calendar).await;
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
    calendar: Calendar,
) -> (DashboardData, String) {
    // Get oracle identity
    let pubkey = state.oracle.public_key_base64();
    let npub = state.oracle.npub();

    // The cards link to the events list, which leaves out unlisted events.
    let (counts, (weather, default_airports)) = tokio::join!(
        state.oracle.event_counts(false),
        get_latest_weather(state, station_ids, start, end, calendar)
    );
    let counts = counts.unwrap_or_else(|error| {
        log::error!("event counts: {error:#}");
        Default::default()
    });
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
            counts,
            weather,
        },
        selection_path,
    )
}

/// Get weather from the latest available observation files. Also says
/// whether these are the default airports, which need not be named.
async fn get_latest_weather(
    state: &Arc<AppState>,
    station_ids: Option<&[String]>,
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
    calendar: Calendar,
) -> (Vec<WeatherDisplay>, bool) {
    if let Some(station_ids) = station_ids {
        let weather_data =
            super::weather::load_weather(state, station_ids, start, end, calendar).await;
        return (weather_data, false);
    }
    // Query only the default airports: a forecast query over every station
    // exceeds the per-query memory limit, which left the forecast column empty.
    let weather_data = super::weather::load_weather(
        state,
        &super::weather::default_airports(),
        start,
        end,
        calendar,
    )
    .await;
    if !weather_data.is_empty() {
        return (weather_data, true);
    }
    // Data without any default airport: show the first 20 stations in it.
    let stations = state.stations().await.unwrap_or_default();
    let mut first: Vec<String> = stations
        .iter()
        .map(|station| station.station_id.clone())
        .collect();
    first.sort();
    first.truncate(20);
    if first.is_empty() {
        return (vec![], false);
    }
    let mut weather_data = super::weather::load_weather(state, &first, start, end, calendar).await;
    weather_data.sort_by(|a, b| a.station_id.cmp(&b.station_id));
    (weather_data, false)
}
