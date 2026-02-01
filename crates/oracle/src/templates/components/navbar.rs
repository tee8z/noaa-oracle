use maud::{html, Markup};

use crate::templates::layouts::CurrentPage;

/// Responsive navigation bar with HTMX-powered navigation
pub fn navbar(current_page: CurrentPage) -> Markup {
    html! {
        nav class="navbar mb-4" role="navigation" aria-label="main navigation" {
            div class="navbar-brand" {
                // Hamburger menu for mobile
                a role="button" class="navbar-burger" aria-label="menu"
                  aria-expanded="false" data-target="navbarMenu" {
                    span aria-hidden="true" {}
                    span aria-hidden="true" {}
                    span aria-hidden="true" {}
                }
            }

            div id="navbarMenu" class="navbar-menu" {
                div class="navbar-start" {
                    a href="/"
                      class=(nav_item_class(current_page, CurrentPage::Dashboard))
                      hx-get="/"
                      hx-target="#main-content"
                      hx-push-url="true"
                      hx-swap="innerHTML" {
                        span class="icon-text" {
                            span class="icon" { (dashboard_icon()) }
                            span { "Dashboard" }
                        }
                    }

                    a href="/events"
                      class=(nav_item_class(current_page, CurrentPage::Events))
                      hx-get="/events"
                      hx-target="#main-content"
                      hx-push-url="true"
                      hx-swap="innerHTML" {
                        span class="icon-text" {
                            span class="icon" { (events_icon()) }
                            span { "Events" }
                        }
                    }

                    a href="/raw"
                      class=(nav_item_class(current_page, CurrentPage::RawData))
                      hx-get="/raw"
                      hx-target="#main-content"
                      hx-push-url="true"
                      hx-swap="innerHTML" {
                        span class="icon-text" {
                            span class="icon" { (data_icon()) }
                            span { "Raw Data" }
                        }
                    }
                }
            }
        }
    }
}

fn nav_item_class(current: CurrentPage, page: CurrentPage) -> &'static str {
    if current == page {
        "navbar-item is-active"
    } else {
        "navbar-item"
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
