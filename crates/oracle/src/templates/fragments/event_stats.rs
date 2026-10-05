use maud::{Markup, html};

use crate::events::EventCounts;

/// Counts and links use the same visibility on each listener.
pub fn event_stats(counts: &EventCounts, operator: bool) -> Markup {
    html! {
        section class="box event-stats" aria-label="Events by status" {
            @for (status, count, label, hint) in [
                ("live", counts.live, "Live", "Accepting entries"),
                ("running", counts.running, "Running", "Observing weather"),
                ("completed", counts.completed, "Completed", "Awaiting signature"),
                ("signed", counts.signed, "Signed", "Attested"),
            ] {
                @let href = format!("/events?status={status}{}", if operator { "&unlisted=show" } else { "" });
                a class="stat-card" href=(href)
                  hx-get=(href)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_count_unlisted_events_and_open_the_list_with_them_shown() {
        let counts = EventCounts {
            live: 2,
            running: 33,
            completed: 145,
            signed: 185,
            unlisted: 290,
        };
        let public = event_stats(&counts, false).into_string();
        assert!(!public.contains("unlisted"));
        let html = event_stats(&counts, true).into_string();
        assert!(html.contains(">33<"), "{html}");
        assert!(
            html.contains("href=\"/events?status=running&amp;unlisted=show\""),
            "{html}"
        );
        assert!(!html.contains("status=live\""), "{html}");
    }
}
