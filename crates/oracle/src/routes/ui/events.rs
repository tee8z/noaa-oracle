use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
};
use serde::Deserialize;
use time::OffsetDateTime;

use super::htmx::{Render, page_or_fragment};
use crate::{
    AppState,
    events::EventFilter,
    templates::{
        fragments::events::{EventFilters, EventView, events_list, events_section},
        pages::events::{events_fragment, events_page},
    },
};

#[derive(Debug, Deserialize, Default)]
pub struct EventsQuery {
    /// `live`, `running`, `completed` or `signed`; empty for all.
    pub status: Option<String>,
    /// `show` to include test events.
    pub tests: Option<String>,
}

/// Handler for the events page (GET /events). The filters replace
/// `#events`; the 30-second refresh replaces `#events-list`.
pub async fn events_handler(
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let events = build_events_view(&state).await;
    let filters = EventFilters::parse(query.status.as_deref(), query.tests.as_deref());
    let now = OffsetDateTime::now_utc();
    page_or_fragment(
        match super::htmx::render(&headers) {
            Render::Page => events_page(&events, filters, now),
            Render::Content => events_fragment(&events, filters, now),
            Render::Part(target) if target == "events-list" => events_list(&events, filters, now),
            Render::Part(_) => events_section(&events, filters, now),
        }
        .into_string(),
    )
}

async fn build_events_view(state: &Arc<AppState>) -> Vec<EventView> {
    let events = state
        .oracle
        .list_events(EventFilter::default())
        .await
        .unwrap_or_default();

    events
        .into_iter()
        .map(|e| EventView {
            id: e.id.to_string(),
            locations: e.locations,
            status: e.status,
            start_observation: e.start_observation_date,
            end_observation: e.end_observation_date,
            signing_date: e.signing_date,
            total_entries: e.total_entries,
            total_allowed_entries: e.total_allowed_entries,
            number_of_places_win: e.number_of_places_win,
        })
        .collect()
}
