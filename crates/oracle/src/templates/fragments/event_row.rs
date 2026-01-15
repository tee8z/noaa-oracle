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
            td class="is-hidden-mobile" {
                span class="is-size-7" {
                    (event.start_observation.clone())
                    br;
                    " to "
                    br;
                    (event.end_observation.clone())
                }
            }

            // Signing date
            td class="is-hidden-mobile" {
                span class="is-size-7" {
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
            td class="has-text-centered is-hidden-mobile" {
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
