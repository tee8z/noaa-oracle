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

/// Whether htmx sent this request.
pub(super) fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// Going back or forward, htmx 4 re-requests the page and replaces the
/// whole body with it.
pub(super) fn is_history_restore(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-history-restore-request")
}

/// A history restore gets the full page like a normal load. htmx 4 names the
/// target `tag#id` (`div#events-list`); a target without an id gets the
/// page's content.
pub(super) fn render(headers: &HeaderMap) -> Render {
    if !is_htmx(headers) || is_history_restore(headers) {
        return Render::Page;
    }
    let target = headers
        .get("hx-target")
        .and_then(|target| target.to_str().ok())
        .and_then(|target| target.split_once('#'))
        .map(|(_, id)| id);
    match target {
        Some(id) if !id.is_empty() && id != "main-content" => Render::Part(id.to_string()),
        _ => Render::Content,
    }
}

/// The same URL returns a page or a fragment, and pages follow the
/// reader's cookies (their time zone and remembered view), so caches must
/// key on the htmx headers and the cookies.
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
                ("hx-target", "main#main-content")
            ])),
            Render::Content
        );
        assert_eq!(
            render(&headers(&[
                ("hx-request", "true"),
                ("hx-target", "div#events-list")
            ])),
            Render::Part("events-list".into())
        );
        // A target without an id.
        assert_eq!(
            render(&headers(&[("hx-request", "true"), ("hx-target", "div")])),
            Render::Content
        );
        assert_eq!(
            render(&headers(&[
                ("hx-request", "true"),
                ("hx-target", "body"),
                ("hx-history-restore-request", "true")
            ])),
            Render::Page
        );
    }
}
