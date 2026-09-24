use axum::{
    http::{HeaderMap, HeaderValue, header},
    response::{Html, IntoResponse, Response},
};

/// True when htmx swaps the response into `#main-content`. A history restore
/// replaces the whole body, so it gets the full page like a normal load.
pub(super) fn wants_fragment(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request") && !headers.contains_key("hx-history-restore-request")
}

/// The same URL returns a page or a fragment, so caches must key on the header.
pub(super) fn page_or_fragment(html: String) -> Response {
    let mut response = Html(html).into_response();
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("HX-Request"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_htmx_swaps_get_fragments() {
        let mut headers = HeaderMap::new();
        assert!(!wants_fragment(&headers));
        headers.insert("hx-request", HeaderValue::from_static("true"));
        assert!(wants_fragment(&headers));
        headers.insert("hx-history-restore-request", HeaderValue::from_static("true"));
        assert!(!wants_fragment(&headers));
    }
}
