use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use uuid::Uuid;

use crate::{templates::event_detail_page, AppState};

/// Handler for the event detail page (GET /events/{id})
pub async fn event_detail_handler(
    State(state): State<Arc<AppState>>,
    Path(event_id): Path<Uuid>,
) -> Response {
    match state.oracle.get_event(&event_id).await {
        Ok(event) => {
            Html(event_detail_page(&state.remote_url, &event).into_string()).into_response()
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            Html(not_found_page(&event_id.to_string())),
        )
            .into_response(),
    }
}

fn not_found_page(event_id: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <title>Event Not Found</title>
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bulma@1.0.4/css/bulma.min.css">
</head>
<body>
    <section class="section">
        <div class="container">
            <div class="notification is-warning">
                <h1 class="title">Event Not Found</h1>
                <p>The event with ID <code>{}</code> could not be found.</p>
                <a href="/events" class="button is-primary mt-4">Back to Events</a>
            </div>
        </div>
    </section>
</body>
</html>"#,
        event_id
    )
}
