//! The Content-Security-Policy for the UI's pages.
//!
//! Scripts load only from this site, as files: no inline scripts, `on…`
//! attributes or `eval`. htmx is configured to match (see the layout's
//! `HTMX_CONFIG`). The API and its docs are not covered.

use axum::{
    extract::Request,
    http::{HeaderValue, header},
    middleware::Next,
    response::Response,
};

pub const PAGE_POLICY: &str = "script-src 'self'; object-src 'none'; base-uri 'none'; \
     frame-ancestors 'none'; form-action 'self'";

/// The raw data page also runs DuckDB-WASM from jsdelivr: its module, the
/// worker it starts from a `blob:` URL, and WebAssembly.
pub const RAW_DATA_POLICY: &str = "script-src 'self' https://cdn.jsdelivr.net 'wasm-unsafe-eval'; \
     worker-src blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; \
     form-action 'self'";

/// Adds [`PAGE_POLICY`] to every UI response that doesn't set its own.
pub async fn content_security_policy(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if !headers.contains_key(header::CONTENT_SECURITY_POLICY) {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(PAGE_POLICY),
        );
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}
