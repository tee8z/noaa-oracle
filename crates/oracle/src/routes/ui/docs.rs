//! GET /docs: the API reference. Scalar draws it from the OpenAPI document
//! in the page, using its bundle from `/assets/` (pinned in
//! `vendor/scalar/`), not a CDN.

use axum::{
    Router,
    http::{HeaderValue, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use utoipa::openapi::OpenApi;
use utoipa_scalar::Scalar;

use super::policy::DOCS_POLICY;
use crate::templates::assets;

/// Scalar's settings: no web fonts from its own site, no telemetry and no
/// AI agent; only what this page serves.
const CONFIGURATION: &str =
    r#"{"withDefaultFonts":false,"telemetry":false,"agent":{"disabled":true}}"#;

/// The page, with `$spec` where Scalar's crate puts the OpenAPI document.
fn template() -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>4cast Truth Oracle - API docs</title>
</head>
<body>
<noscript><p>The API reference needs JavaScript.</p></noscript>
<script id="api-reference" type="application/json" data-configuration='{CONFIGURATION}'>
$spec
</script>
<script src="{scalar}"></script>
</body>
</html>
"#,
        scalar = assets::SCALAR_JS.url,
    )
}

/// The `/docs` route, rendered once.
pub fn docs_router<S>(spec: OpenApi) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let html = Scalar::new(spec).custom_html(template()).to_html();
    Router::new().route(
        "/docs",
        get(move || {
            let html = html.clone();
            async move { docs_response(html) }
        }),
    )
}

fn docs_response(html: String) -> Response {
    let mut response = Html(html).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(DOCS_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}
