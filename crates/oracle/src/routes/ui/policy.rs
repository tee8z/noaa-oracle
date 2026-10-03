//! The Content-Security-Policy for the UI's pages.
//!
//! Scripts load only from this site, as files: no inline scripts, `on…`
//! attributes or `eval`. htmx 4 has no setting that stops it evaluating
//! `hx-on` or running scripts in swapped HTML, so this policy is what stops
//! them. Pages also require Trusted Types: only the `htmx` policy
//! (`layouts/htmx_security.js`) may turn strings into HTML. Styles, images,
//! fonts and requests come only from this site too. The API reference at
//! `/docs` has its own policy ([`DOCS_POLICY`]); the JSON API is not covered.

use axum::{
    extract::Request,
    http::{HeaderValue, header},
    middleware::Next,
    response::Response,
};

pub const PAGE_POLICY: &str = "script-src 'self'; default-src 'self'; style-src 'self'; \
     img-src 'self' data:; object-src 'none'; base-uri 'none'; \
     frame-ancestors 'none'; form-action 'self'; \
     require-trusted-types-for 'script'; trusted-types htmx";

/// The API reference. Scalar is served from this site like the UI's own
/// scripts, with its web fonts and telemetry turned off, so nothing loads
/// from elsewhere. It adds its styles as a `<style>` element and sets
/// `style` attributes, and its request client calls this API. Its Vue
/// rendering writes HTML strings, so this page does not require Trusted
/// Types.
pub const DOCS_POLICY: &str = "script-src 'self'; default-src 'self'; \
     style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; \
     worker-src 'self' blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; \
     form-action 'self'";

/// The raw data page imports its pinned DuckDB/Arrow module from this site.
/// DuckDB still loads its pinned worker script and WebAssembly from jsDelivr.
/// It starts that worker from a `blob:` URL containing importScripts, so this
/// page does not require Trusted Types.
pub const RAW_DATA_POLICY: &str = "script-src 'self' 'wasm-unsafe-eval' \
     https://cdn.jsdelivr.net/npm/@duckdb/duckdb-wasm@1.29.0/; \
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
