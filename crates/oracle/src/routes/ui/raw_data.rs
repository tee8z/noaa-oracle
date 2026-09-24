use std::sync::Arc;

use axum::{extract::State, http::HeaderMap, response::Response};

use super::htmx::{page_or_fragment, wants_fragment};
use crate::{
    AppState,
    templates::{pages::raw_data::raw_data_content, raw_data_page},
};

/// Handler for the raw data page (GET /raw)
/// Returns full page for normal requests, content only for HTMX requests
pub async fn raw_data_handler(headers: HeaderMap, State(state): State<Arc<AppState>>) -> Response {
    page_or_fragment(if wants_fragment(&headers) {
        raw_data_content().into_string()
    } else {
        raw_data_page(&state.remote_url).into_string()
    })
}
