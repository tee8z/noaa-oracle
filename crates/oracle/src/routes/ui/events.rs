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
    events::{EventListQuery, EventSummary},
    templates::{
        fragments::events::{
            EventFilters, EventView, EventsPage, PAGE_SIZE, events_list, events_section,
        },
        pages::events::{events_fragment, events_page},
    },
};

#[derive(Debug, Deserialize, Default)]
pub struct EventsQuery {
    /// `live`, `running`, `completed` or `signed`; empty for all.
    pub status: Option<String>,
    /// `show` to include unlisted events.
    pub unlisted: Option<String>,
    /// Event id: show the page of events created before it.
    pub before: Option<String>,
}

/// Handler for the events page (GET /events). The filters replace
/// `#events`; the 30-second refresh and the page links replace
/// `#events-list`.
pub async fn events_handler(
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let filters = EventFilters::parse(
        query.status.as_deref(),
        query.unlisted.as_deref(),
        query.before.as_deref(),
    );
    let list = EventListQuery {
        status: filters.status,
        include_unlisted: filters.show_unlisted,
        before: filters.before,
        // One more than a page says whether an older page exists.
        limit: PAGE_SIZE + 1,
    };
    let (events, counts) = tokio::join!(
        state.oracle.event_page(&list),
        state.oracle.event_counts(filters.show_unlisted)
    );
    let mut events = events.unwrap_or_else(|error| {
        log::error!("events page: {error:#}");
        vec![]
    });
    let counts = counts.unwrap_or_else(|error| {
        log::error!("event counts: {error:#}");
        Default::default()
    });
    let older = (events.len() > PAGE_SIZE).then(|| {
        events.truncate(PAGE_SIZE);
        events.last().map(|event| event.id)
    });
    let views: Vec<EventView> = events.into_iter().map(view).collect();
    let page = EventsPage {
        events: &views,
        counts,
        filters,
        older: older.flatten(),
        now: OffsetDateTime::now_utc(),
    };
    page_or_fragment(
        match super::htmx::render(&headers) {
            Render::Page => events_page(&page),
            Render::Content => events_fragment(&page),
            Render::Part(target) if target == "events-list" => events_list(&page),
            Render::Part(_) => events_section(&page),
        }
        .into_string(),
    )
}

fn view(event: EventSummary) -> EventView {
    EventView {
        id: event.id.to_string(),
        locations: event.locations,
        status: event.status,
        start_observation: event.start_observation_date,
        end_observation: event.end_observation_date,
        signing_date: event.signing_date,
        total_entries: event.total_entries,
        total_allowed_entries: event.total_allowed_entries,
        number_of_places_win: event.number_of_places_win,
        unlisted: event.unlisted,
    }
}
