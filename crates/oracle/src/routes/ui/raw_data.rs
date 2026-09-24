use axum::{
    http::{HeaderMap, HeaderName, HeaderValue, header},
    response::{Html, IntoResponse, Response},
};
use time::OffsetDateTime;

use super::{htmx::is_htmx, policy::RAW_DATA_POLICY};
use crate::templates::pages::raw_data::raw_data_page;

/// Handler for the raw data page (GET /raw). Its policy lets the page's
/// script run DuckDB-WASM, so it always loads as a whole document: the tab
/// is a plain link, and when htmx asks for it (going back to it from a page
/// opened by htmx) the reply tells htmx to reload instead.
pub async fn raw_data_handler(headers: HeaderMap) -> Response {
    if is_htmx(&headers) {
        return ([(HeaderName::from_static("hx-refresh"), "true")], Html("")).into_response();
    }
    let mut response = Html(raw_data_page(OffsetDateTime::now_utc()).into_string()).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(RAW_DATA_POLICY),
    );
    headers.insert(header::VARY, HeaderValue::from_static("HX-Request"));
    response
}
