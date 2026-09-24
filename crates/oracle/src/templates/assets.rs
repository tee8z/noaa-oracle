//! The UI's stylesheet, script and images, embedded at build time and served
//! at content-hashed URLs. A changed file gets a new URL, so browsers may
//! cache every response for a year.

use axum::{
    Router,
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

const CACHE_POLICY: &str = "public, max-age=31536000, immutable";

/// Routes for every embedded asset. Unknown hashes are not found.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(CSS_URL, get(|| asset("text/css; charset=utf-8", CSS_BYTES)))
        .route(
            JS_URL,
            get(|| asset("text/javascript; charset=utf-8", JS_BYTES)),
        )
        .route(USA_MAP_URL, get(|| asset("image/svg+xml", USA_MAP_BYTES)))
}

async fn asset(content_type: &'static str, bytes: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, CACHE_POLICY),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::*;

    async fn request(method: Method, url: &str) -> Response {
        router::<()>()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(url)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn hashed_urls_serve_embedded_bytes_with_immutable_cache_headers() {
        for (url, content_type, bytes) in [
            (CSS_URL, "text/css; charset=utf-8", CSS_BYTES),
            (JS_URL, "text/javascript; charset=utf-8", JS_BYTES),
            (USA_MAP_URL, "image/svg+xml", USA_MAP_BYTES),
        ] {
            let response = request(Method::GET, url).await;
            assert_eq!(response.status(), StatusCode::OK, "{url}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
            assert_eq!(response.headers()[header::CACHE_CONTROL], CACHE_POLICY);
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(!body.is_empty());
            assert_eq!(body.as_ref(), bytes);
        }
    }

    #[tokio::test]
    async fn only_current_hashes_are_served() {
        for url in [
            "/assets/site.0000000000000000.css",
            "/assets/site.css",
            "/assets/site.js",
            "/static/app.min.js",
        ] {
            let response = request(Method::GET, url).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{url}");
            assert!(!response.headers().contains_key(header::CACHE_CONTROL));
        }
    }

    #[test]
    fn urls_carry_a_content_hash() {
        for url in [CSS_URL, JS_URL, USA_MAP_URL] {
            let hash = url.rsplit('.').nth(1).unwrap();
            assert_eq!(hash.len(), 16, "{url}");
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{url}");
        }
    }
}
