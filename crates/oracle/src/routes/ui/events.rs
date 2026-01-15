use std::sync::Arc;

use axum::{extract::State, response::Html};
use time::format_description::well_known::Rfc3339;

use crate::{
    db::EventFilter,
    templates::{events_page, events_table_rows, EventView},
    AppState,
};

/// Handler for the events page (GET /events)
pub async fn events_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let events = build_events_view(&state).await;
    Html(events_page(&state.remote_url, &events).into_string())
}

/// Handler for events table rows only (HTMX partial for auto-refresh)
pub async fn events_rows_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let events = build_events_view(&state).await;
    Html(events_table_rows(&events).into_string())
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
            start_observation: e
                .start_observation_date
                .format(&Rfc3339)
                .unwrap_or_else(|_| "Invalid".to_string()),
            end_observation: e
                .end_observation_date
                .format(&Rfc3339)
                .unwrap_or_else(|_| "Invalid".to_string()),
            signing_date: e
                .signing_date
                .format(&Rfc3339)
                .unwrap_or_else(|_| "Invalid".to_string()),
            total_entries: e.total_entries,
            total_allowed_entries: e.total_allowed_entries,
            number_of_places_win: e.number_of_places_win,
        })
        .collect()
}
