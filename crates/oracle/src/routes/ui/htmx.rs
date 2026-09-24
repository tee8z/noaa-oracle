//! One URL serves a page and its parts: a normal load gets the whole page,
//! an htmx swap into `#main-content` gets the page's content, and a swap
//! into a smaller target gets only that part.

use axum::{
    http::{HeaderMap, HeaderName, HeaderValue, header},
    response::{Html, IntoResponse, Response},
};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Render {
    /// The whole document.
    Page,
    /// What goes into `#main-content`.
    Content,
    /// Only the element with this id.
    Part(String),
}

/// A history restore replaces the whole body, so it gets the full page like
/// a normal load.
pub(super) fn render(headers: &HeaderMap) -> Render {
    if !headers.contains_key("hx-request") || headers.contains_key("hx-history-restore-request") {
        return Render::Page;
    }
    match headers
        .get("hx-target")
        .and_then(|target| target.to_str().ok())
    {
        Some(target) if !target.is_empty() && target != "main-content" => {
            Render::Part(target.to_string())
        }
        _ => Render::Content,
    }
}

/// The same URL returns a page or a fragment, so caches must key on the
/// htmx headers.
pub(super) fn page_or_fragment(html: String) -> Response {
    let mut response = Html(html).into_response();
    response.headers_mut().insert(
        header::VARY,
        HeaderValue::from_static("HX-Request, HX-Target, HX-History-Restore-Request, Cookie"),
    );
    response
}

/// Tells htmx which page URL shows what this fragment shows.
pub(super) fn with_url(mut response: Response, name: &'static str, url: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(url) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(name), value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn only_htmx_swaps_get_fragments() {
        assert_eq!(render(&headers(&[])), Render::Page);
        assert_eq!(render(&headers(&[("hx-request", "true")])), Render::Content);
        assert_eq!(
            render(&headers(&[
                ("hx-request", "true"),
                ("hx-target", "main-content")
            ])),
            Render::Content
        );
        assert_eq!(
            render(&headers(&[
                ("hx-request", "true"),
                ("hx-target", "events-list")
            ])),
            Render::Part("events-list".into())
        );
        assert_eq!(
            render(&headers(&[
                ("hx-request", "true"),
                ("hx-target", "events-list"),
                ("hx-history-restore-request", "true")
            ])),
            Render::Page
        );
    }
}
