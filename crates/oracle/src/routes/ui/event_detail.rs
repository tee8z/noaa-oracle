use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use super::htmx::{Render, page_or_fragment};
use crate::{
    AppState,
    templates::pages::event_detail::{
        event_detail_fragment, event_detail_page, event_not_found_fragment, event_not_found_page,
        event_unavailable_fragment, event_unavailable_page,
    },
};

/// Handler for the event detail page (GET /events/{id}).
/// Returns the full page for normal requests and only the content for htmx,
/// which swaps it into the page's existing layout.
pub async fn event_detail_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Path(event_id): Path<Uuid>,
) -> Response {
    match state.oracle.get_event(event_id).await {
        Ok(event) => {
            let now = time::OffsetDateTime::now_utc();
            page_or_fragment(
                match super::htmx::render(&headers) {
                    Render::Page => event_detail_page(&event, now),
                    _ => event_detail_fragment(&event, now),
                }
                .into_string(),
            )
        }
        Err(crate::oracle::Error::EventNotFound(_)) => {
            let html = match super::htmx::render(&headers) {
                Render::Page => event_not_found_page(event_id),
                _ => event_not_found_fragment(event_id),
            };
            (StatusCode::NOT_FOUND, page_or_fragment(html.into_string())).into_response()
        }
        Err(error) => {
            log::error!("event page {event_id}: {error:#}");
            let html = match super::htmx::render(&headers) {
                Render::Page => event_unavailable_page(event_id),
                _ => event_unavailable_fragment(event_id),
            };
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                page_or_fragment(html.into_string()),
            )
                .into_response()
        }
    }
}
