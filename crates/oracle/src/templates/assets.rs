//! The UI's stylesheet, scripts and images, bundled by `build.rs` and
//! embedded in the binary.
//!
//! Each asset's URL contains a hash of its bytes, so a URL never changes
//! meaning and browsers may cache it for a year. Any other `/assets/` path,
//! including an old hash, is not found.

use axum::{
    extract::Path,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

/// One embedded file: its hashed URL, type, and bytes (plain and gzipped).
pub struct Asset {
    pub url: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
    pub gzip: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

const CACHE_POLICY: &str = "public, max-age=31536000, immutable";

/// The asset served at `/assets/{file}`.
pub fn find(file: &str) -> Option<&'static Asset> {
    ALL.iter()
        .find(|asset| asset.url.strip_prefix("/assets/") == Some(file))
}

/// GET /assets/{file}: gzipped when the browser accepts it.
pub async fn serve_asset(Path(file): Path<String>, headers: HeaderMap) -> Response {
    let Some(asset) = find(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let accepts_gzip = headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|coding| {
            let mut parts = coding.split(';').map(str::trim);
            parts.next() == Some("gzip") && !parts.any(|parameter| parameter == "q=0")
        });
    let mut response = if accepts_gzip {
        ([(header::CONTENT_ENCODING, "gzip")], asset.gzip).into_response()
    } else {
        asset.bytes.into_response()
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_POLICY),
    );
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::Request,
        routing::get,
    };
    use flate2::read::GzDecoder;
    use tower::ServiceExt;

    use super::*;

    async fn request(url: &str, accept_encoding: Option<&str>) -> Response {
        let mut request = Request::get(url);
        if let Some(value) = accept_encoding {
            request = request.header(header::ACCEPT_ENCODING, value);
        }
        Router::new()
            .route("/assets/{file}", get(serve_asset))
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn hashed_urls_serve_embedded_bytes_with_immutable_cache_headers() {
        for asset in ALL {
            let response = request(asset.url, None).await;
            assert_eq!(response.status(), StatusCode::OK, "{}", asset.url);
            assert_eq!(response.headers()[header::CONTENT_TYPE], asset.content_type);
            assert_eq!(response.headers()[header::CACHE_CONTROL], CACHE_POLICY);
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(!body.is_empty(), "{}", asset.url);
            assert_eq!(body.as_ref(), asset.bytes);
        }
    }

    #[tokio::test]
    async fn browsers_that_accept_gzip_get_the_same_bytes_compressed() {
        for asset in ALL {
            let response = request(asset.url, Some("br, gzip;q=0.8")).await;
            assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
            assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let mut plain = Vec::new();
            GzDecoder::new(body.as_ref())
                .read_to_end(&mut plain)
                .unwrap();
            assert_eq!(plain, asset.bytes, "{}", asset.url);
        }
        let refused = request(SITE_JS.url, Some("gzip;q=0, identity")).await;
        assert!(!refused.headers().contains_key(header::CONTENT_ENCODING));
    }

    #[tokio::test]
    async fn only_current_hashes_are_served() {
        for url in [
            "/assets/site.0000000000000000.css",
            "/assets/site.css",
            "/assets/site.js",
            "/assets/..%2FCargo.toml",
        ] {
            let response = request(url, None).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{url}");
            assert!(!response.headers().contains_key(header::CACHE_CONTROL));
        }
    }

    #[test]
    fn urls_carry_a_content_hash() {
        for asset in ALL {
            let hash = asset.url.rsplit('.').nth(1).unwrap();
            assert_eq!(hash.len(), 16, "{}", asset.url);
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{}", asset.url);
        }
    }

    #[test]
    fn only_the_raw_data_page_loads_duckdb() {
        let site = std::str::from_utf8(SITE_JS.bytes).unwrap();
        assert!(!site.contains("duckdb"));
        let raw_data = std::str::from_utf8(RAW_DATA_JS.bytes).unwrap();
        assert!(raw_data.contains("duckdb"));
    }
}
