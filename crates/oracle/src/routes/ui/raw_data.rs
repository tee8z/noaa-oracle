use axum::{http::HeaderMap, response::Response};
use time::OffsetDateTime;

use super::htmx::{Render, page_or_fragment};
use crate::templates::pages::raw_data::{raw_data_fragment, raw_data_page};

/// Handler for the raw data page (GET /raw)
/// Returns full page for normal requests, content only for HTMX requests
pub async fn raw_data_handler(headers: HeaderMap) -> Response {
    let now = OffsetDateTime::now_utc();
    page_or_fragment(
        match super::htmx::render(&headers) {
            Render::Page => raw_data_page(now),
            _ => raw_data_fragment(now),
        }
        .into_string(),
    )
}
