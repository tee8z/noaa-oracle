use maud::{Markup, html};

use crate::templates::layouts::CurrentPage;

/// The three pages, as tabs that fit a phone screen. htmx swaps only the
/// main content; the response re-sends the tabs out of band so the current
/// page stays marked. The raw data page loads in full, because it needs its
/// own script and a Content-Security-Policy that allows DuckDB-WASM.
pub fn tabs(current: CurrentPage, out_of_band: bool) -> Markup {
    html! {
        nav id="site-tabs" class="tabs site-tabs" aria-label="Pages"
            hx-swap-oob=[out_of_band.then_some("true")] {
            ul {
                (tab(current, CurrentPage::Dashboard, "/", "Dashboard", dashboard_icon()))
                (tab(current, CurrentPage::Events, "/events", "Events", events_icon()))
                (tab(current, CurrentPage::RawData, "/raw", "Raw data", data_icon()))
            }
        }
    }
}

fn tab(current: CurrentPage, page: CurrentPage, href: &str, label: &str, icon: Markup) -> Markup {
    let active = current == page;
    let swap = page != CurrentPage::RawData;
    html! {
        li class=[active.then_some("is-active")] {
            a href=(href)
              hx-get=[swap.then_some(href)]
              hx-target=[swap.then_some("#main-content")]
              hx-push-url=[swap.then_some("true")]
              aria-current=[active.then_some("page")] {
                span class="icon is-small" aria-hidden="true" { (icon) }
                span { (label) }
            }
        }
    }
}

fn dashboard_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            rect x="3" y="3" width="7" height="7" {}
            rect x="14" y="3" width="7" height="7" {}
            rect x="14" y="14" width="7" height="7" {}
            rect x="3" y="14" width="7" height="7" {}
        }
    }
}

fn events_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            rect x="3" y="4" width="18" height="18" rx="2" ry="2" {}
            line x1="16" y1="2" x2="16" y2="6" {}
            line x1="8" y1="2" x2="8" y2="6" {}
            line x1="3" y1="10" x2="21" y2="10" {}
        }
    }
}

fn data_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            ellipse cx="12" cy="5" rx="9" ry="3" {}
            path d="M21 12c0 1.66-4 3-9 3s-9-1.34-9-3" {}
            path d="M3 5v14c0 1.66 4 3 9 3s9-1.34 9-3V5" {}
        }
    }
}
