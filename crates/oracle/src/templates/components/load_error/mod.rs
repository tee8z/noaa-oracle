//! A part of the page that could not be loaded: what failed and a button
//! that asks again. The server sends it when a query fails or times out;
//! `load_error.js` shows the same when a request gets no reply at all, in
//! any target marked with `data-load-error`.

use maud::{Markup, html};

/// `target` is the htmx target for the retry's reply, which replaces the
/// target's content.
pub fn load_error(message: &str, retry_url: &str, target: &str) -> Markup {
    html! {
        div class="load-error" role="alert" {
            p { (message) }
            button type="button" class="button is-small"
                hx-get=(retry_url) hx-target=(target) hx-swap="innerHTML" {
                "Try again"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retry_asks_again_into_the_same_place() {
        let html = load_error(
            "Couldn't load the forecast.",
            "/fragments/forecast/KORD",
            "closest .wx-forecast",
        )
        .into_string();
        assert!(html.contains("role=\"alert\""));
        assert!(html.contains("hx-get=\"/fragments/forecast/KORD\""));
        assert!(html.contains("hx-target=\"closest .wx-forecast\""));
    }
}
