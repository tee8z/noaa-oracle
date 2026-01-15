use maud::{html, Markup};

use super::event_row::{event_row, EventView};

/// Events table fragment
/// Shows all events with auto-refresh capability
pub fn events_table(events: &[EventView]) -> Markup {
    html! {
        div class="box" {
            div class="is-flex is-justify-content-space-between is-align-items-center mb-4 is-flex-wrap-wrap" {
                h2 class="title is-5 mb-0" { "Oracle Events" }

                // Manual refresh button
                button class="button is-small is-light"
                       hx-get="/fragments/events-rows"
                       hx-target="#events-tbody"
                       hx-swap="innerHTML" {
                    span class="icon is-small" {
                        (refresh_icon())
                    }
                    span { "Refresh" }
                }
            }

            @if events.is_empty() {
                div class="has-text-centered has-text-grey py-6" {
                    p class="is-size-5" { "No events found" }
                    p class="is-size-7" { "Events will appear here when created by coordinators." }
                }
            } @else {
                div class="table-container" {
                    table class="table is-fullwidth is-striped is-hoverable" {
                        thead {
                            tr {
                                th { "ID" }
                                th { "Locations" }
                                th { "Status" }
                                th class="is-hidden-mobile" { "Observation Window" }
                                th class="is-hidden-mobile" { "Signing Date" }
                                th class="has-text-centered" { "Entries" }
                                th class="has-text-centered is-hidden-mobile" { "Winners" }
                                th { "" }
                            }
                        }
                        tbody id="events-tbody"
                              hx-get="/fragments/events-rows"
                              hx-trigger="every 30s"
                              hx-swap="innerHTML" {
                            @for event in events {
                                (event_row(event))
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Just the table rows - used for HTMX partial updates
pub fn events_table_rows(events: &[EventView]) -> Markup {
    html! {
        @for event in events {
            (event_row(event))
        }
    }
}

fn refresh_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            polyline points="23 4 23 10 17 10" {}
            polyline points="1 20 1 14 7 14" {}
            path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15" {}
        }
    }
}
