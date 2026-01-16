use std::sync::Arc;

use axum::{extract::State, http::HeaderMap, response::Html};

use crate::{
    templates::{pages::raw_data::raw_data_content, raw_data_page},
    AppState,
};

/// Handler for the raw data page (GET /raw)
/// Returns full page for normal requests, content only for HTMX requests
pub async fn raw_data_handler(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Html<String> {
    // Check if this is an HTMX request
    if headers.contains_key("hx-request") {
        // Return only the content for HTMX partial updates
        Html(raw_data_content().into_string())
    } else {
        // Return full page for normal browser requests
        Html(raw_data_page(&state.remote_url).into_string())
    }
}
