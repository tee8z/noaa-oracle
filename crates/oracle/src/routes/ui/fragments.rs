use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use maud::{PreEscaped, html};
use serde::Deserialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{
    forecast::forecast_html,
    htmx::{Render, page_or_fragment, with_url},
    local_day::{reader_calendar, vary_on_cookie},
};
use crate::{
    AppState,
    templates::{
        components::load_error::load_error,
        fragments::{
            WeatherContext, station_detail, weather::place_name, weather_list, weather_section,
        },
    },
    weather_data::validate_station_id,
};

#[derive(Debug, Deserialize)]
pub struct WeatherQuery {
    pub stations: Option<String>,
    pub add_station: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    /// `map` or `list`
    pub view: Option<String>,
    /// Station search in the list.
    pub q: Option<String>,
}

/// Handler for the weather section (GET /fragments/weather): the Map/List
/// tabs, adding a station and the five-minute refresh get the section; a
/// search (`HX-Target: weather-list`) gets only the list.
pub async fn weather_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<WeatherQuery>,
) -> Response {
    let add_station = query
        .add_station
        .as_deref()
        .filter(|station| validate_station_id(station).is_ok());
    let named = super::weather::requested_stations(query.stations.as_deref());
    let requested = named.is_some() || add_station.is_some();
    let mut station_ids = named.unwrap_or_else(super::weather::default_airports);
    if let Some(add_station) = add_station
        && !station_ids.iter().any(|station| station == add_station)
    {
        station_ids.push(add_station.to_string());
    }

    let start = query
        .start
        .as_deref()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok());
    let end = query
        .end
        .as_deref()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok());
    let (weather, stations) = tokio::join!(
        super::weather::load_weather(
            &state,
            &station_ids,
            start,
            end,
            super::local_day::reader_calendar(&headers),
        ),
        state.stations()
    );
    let stations = stations.unwrap_or_default();
    // The default airports refresh without naming them.
    let selection: &[String] = if requested { &station_ids } else { &[] };
    let selection_path = super::weather::refresh_path(selection, start, end);
    let view = super::dashboard::chosen_view(query.view.as_deref(), &headers);
    let context = WeatherContext {
        view,
        query: query.q.as_deref().unwrap_or_default(),
        selection_path: &selection_path,
        stations: &stations,
        now: OffsetDateTime::now_utc(),
    };
    let only_list = super::htmx::render(&headers) == Render::Part("weather-list".into());
    let mut response = page_or_fragment(if only_list {
        weather_list(&weather, &context).into_string()
    } else {
        weather_section(&weather, &context).into_string()
    });
    if query.view.is_some() {
        super::dashboard::remember_view(&mut response, view);
    }
    if !headers.contains_key("hx-request") {
        response
    } else if add_station.is_some() {
        with_url(response, "hx-push-url", &context.page_url(view))
    } else if only_list {
        with_url(response, "hx-replace-url", &context.page_url(view))
    } else {
        response
    }
}

/// A map pin's station (GET /fragments/station/{id}): its name, then the
/// same cached forecast detail the list shows.
pub async fn station_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Path(station_id): Path<String>,
) -> Response {
    if validate_station_id(&station_id).is_err() {
        return (StatusCode::BAD_REQUEST, "invalid station id").into_response();
    }
    // The station list is cached, so an unknown station costs no query. If
    // the list can't be read, the forecast query decides.
    let place = match state.stations().await {
        Ok(stations) => match stations
            .iter()
            .find(|station| station.station_id == station_id)
        {
            Some(station) => Some(place_name(&station.station_name, &station.state)),
            None => return unknown_station(&station_id),
        },
        Err(_) => None,
    };
    let forecast = forecast_html(&state, &station_id, reader_calendar(&headers)).await;
    let (status, detail) = match forecast {
        Ok(html) => (StatusCode::OK, PreEscaped(html)),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            load_error(
                "Couldn't load this station's forecast and history.",
                &format!("/fragments/station/{station_id}"),
                "#map-station",
            ),
        ),
    };
    let mut response = (
        status,
        Html(station_detail(&station_id, place.as_deref(), detail).into_string()),
    )
        .into_response();
    vary_on_cookie(response.headers_mut());
    response
}

/// Handler for forecast detail fragment (GET /fragments/forecast/:station_id),
/// which a list row loads when it opens. Days are the reader's.
pub async fn forecast_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Path(station_id): Path<String>,
) -> Response {
    if validate_station_id(&station_id).is_err() {
        return (StatusCode::BAD_REQUEST, "invalid station id").into_response();
    }
    if let Ok(stations) = state.stations().await
        && !stations
            .iter()
            .any(|station| station.station_id == station_id)
    {
        return unknown_station(&station_id);
    }
    let mut response = match forecast_html(&state, &station_id, reader_calendar(&headers)).await {
        Ok(html) => Html(html).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(
                load_error(
                    "Couldn't load the forecast and history.",
                    &format!("/fragments/forecast/{station_id}"),
                    "closest .wx-forecast",
                )
                .into_string(),
            ),
        )
            .into_response(),
    };
    vary_on_cookie(response.headers_mut());
    response
}

/// A well-formed id that no station in the data has.
fn unknown_station(station_id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(
            html! { p class="muted" { "No station " code { (station_id) } " in the data." } }
                .into_string(),
        ),
    )
        .into_response()
}
