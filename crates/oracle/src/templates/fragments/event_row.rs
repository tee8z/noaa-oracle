use maud::{html, Markup};

use crate::db::EventStatus;

/// Event view data for table display
pub struct EventView {
    pub id: String,
    pub locations: Vec<String>,
    pub status: EventStatus,
    pub start_observation: String,
    pub end_observation: String,
    pub signing_date: String,
    pub total_entries: i64,
    pub total_allowed_entries: i64,
    pub number_of_places_win: i64,
}

/// Single event row for the events table
pub fn event_row(event: &EventView) -> Markup {
    html! {
        tr class="is-clickable"
           hx-get=(format!("/events/{}", event.id))
           hx-target="#main-content"
           hx-push-url="true" {
            // ID (truncated)
            td {
                code class="is-size-7" title=(event.id.clone()) {
                    (truncate_id(&event.id))
                }
            }

            // Locations
            td {
                div class="tags are-small" {
                    @for location in &event.locations {
                        span class="tag is-light" { (location) }
                    }
                }
            }

            // Status badge
            td {
                span class=(status_class(&event.status)) {
                    (status_text(&event.status))
                }
            }

            // Observation window
            td {
                span class="is-size-7 local-time" data-utc=(event.start_observation.clone()) {
                    (event.start_observation.clone())
                }
                br;
                " to "
                br;
                span class="is-size-7 local-time" data-utc=(event.end_observation.clone()) {
                    (event.end_observation.clone())
                }
            }

            // Signing date
            td {
                span class="is-size-7 local-time" data-utc=(event.signing_date.clone()) {
                    (event.signing_date.clone())
                }
            }

            // Entries
            td class="has-text-centered" {
                (event.total_entries)
                " / "
                (event.total_allowed_entries)
            }

            // Winners
            td class="has-text-centered" {
                (event.number_of_places_win)
            }

            // Action button
            td {
                button class="button is-small is-info is-light" {
                    "View"
                }
            }
        }
    }
}

/// Single event card for mobile view
pub fn event_card(event: &EventView) -> Markup {
    html! {
        div class="event-card box mb-3 is-clickable"
            hx-get=(format!("/events/{}", event.id))
            hx-target="#main-content"
            hx-push-url="true" {
            div class="is-flex is-justify-content-space-between is-align-items-center mb-2" {
                code class="is-size-7" title=(event.id.clone()) {
                    (truncate_id(&event.id))
                }
                span class=(status_class(&event.status)) {
                    (status_text(&event.status))
                }
            }

            div class="tags are-small mb-2" {
                @for location in &event.locations {
                    span class="tag is-light" { (location) }
                }
            }

            div class="is-flex is-flex-wrap-wrap" style="gap: 0.75rem;" {
                div {
                    p class="event-card-label" { "Observation" }
                    p class="is-size-7" {
                        span class="local-time" data-utc=(event.start_observation.clone()) {
                            (event.start_observation.clone())
                        }
                    }
                    p class="is-size-7" {
                        "to "
                        span class="local-time" data-utc=(event.end_observation.clone()) {
                            (event.end_observation.clone())
                        }
                    }
                }
                div {
                    p class="event-card-label" { "Signing" }
                    p class="is-size-7" {
                        span class="local-time" data-utc=(event.signing_date.clone()) {
                            (event.signing_date.clone())
                        }
                    }
                }
            }

            div class="is-flex is-justify-content-space-between is-align-items-center mt-2 pt-2" style="border-top: 1px solid var(--bulma-border);" {
                div class="is-flex" style="gap: 1rem;" {
                    span class="is-size-7" {
                        strong { "Entries: " }
                        (event.total_entries) " / " (event.total_allowed_entries)
                    }
                    span class="is-size-7" {
                        strong { "Winners: " }
                        (event.number_of_places_win)
                    }
                }
                button class="button is-small is-info is-light" {
                    "View"
                }
            }
        }
    }
}

fn truncate_id(id: &str) -> String {
    if id.len() > 8 {
        format!("{}...", &id[..8])
    } else {
        id.to_string()
    }
}

fn status_class(status: &EventStatus) -> &'static str {
    match status {
        EventStatus::Live => "tag is-live",
        EventStatus::Running => "tag is-running",
        EventStatus::Completed => "tag is-completed",
        EventStatus::Signed => "tag is-signed",
    }
}

fn status_text(status: &EventStatus) -> &'static str {
    match status {
        EventStatus::Live => "Live",
        EventStatus::Running => "Running",
        EventStatus::Completed => "Completed",
        EventStatus::Signed => "Signed",
    }
}
