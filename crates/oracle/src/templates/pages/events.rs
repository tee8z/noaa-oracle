use maud::{html, Markup};

use crate::templates::{
    fragments::{events_table, EventView},
    layouts::{base, CurrentPage, PageConfig},
};

/// Events page - shows list of all oracle events
pub fn events_page(api_base: &str, events: &[EventView]) -> Markup {
    let config = PageConfig {
        title: "4cast Truth Oracle - Events",
        api_base,
        current_page: CurrentPage::Events,
    };

    base(&config, events_content(events))
}

/// Events content - can be used for full page or HTMX partial
pub fn events_content(events: &[EventView]) -> Markup {
    html! {
        (events_table(events))
    }
}
