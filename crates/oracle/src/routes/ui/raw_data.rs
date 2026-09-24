use axum::{
    http::{HeaderMap, HeaderValue, header},
    response::Response,
};
use time::OffsetDateTime;

use super::{
    htmx::{Render, page_or_fragment},
    policy::RAW_DATA_POLICY,
};
use crate::templates::pages::raw_data::{raw_data_fragment, raw_data_page};

/// Handler for the raw data page (GET /raw). Its policy lets the page's
/// script run DuckDB-WASM.
pub async fn raw_data_handler(headers: HeaderMap) -> Response {
    let now = OffsetDateTime::now_utc();
    let mut response = page_or_fragment(
        match super::htmx::render(&headers) {
            Render::Page => raw_data_page(now),
            _ => raw_data_fragment(now),
        }
        .into_string(),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(RAW_DATA_POLICY),
    );
    response
}
