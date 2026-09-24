use maud::{Markup, html};

use crate::events::EventCounts;

/// One line of counts, without test events; each opens the events list
/// filtered to that status, which hides test events too.
pub fn event_stats(counts: &EventCounts) -> Markup {
    html! {
        section class="box event-stats" aria-label="Events by status" {
            @for (status, count, label, hint) in [
                ("live", counts.live, "Live", "Accepting entries"),
                ("running", counts.running, "Running", "Observing weather"),
                ("completed", counts.completed, "Completed", "Awaiting signature"),
                ("signed", counts.signed, "Signed", "Attested"),
            ] {
                a class="stat-card" href=(format!("/events?status={status}"))
                  hx-get=(format!("/events?status={status}"))
                  hx-target="#main-content"
                  hx-push-url="true"
                  title=(hint) {
                    span class={ "stat-value stat-" (status) } { (count) }
                    span class="stat-label" { (label) }
                }
            }
        }
    }
}
