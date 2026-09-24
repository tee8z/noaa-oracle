use maud::{Markup, html};

/// Event counts by status.
#[derive(Default)]
pub struct EventStats {
    pub live_count: usize,
    pub running_count: usize,
    pub completed_count: usize,
    pub signed_count: usize,
}

/// One line of counts; each opens the events list filtered to that status.
pub fn event_stats(stats: &EventStats) -> Markup {
    html! {
        section class="box event-stats" aria-label="Events by status" {
            @for (status, count, label, hint) in [
                ("live", stats.live_count, "Live", "Accepting entries"),
                ("running", stats.running_count, "Running", "Observing weather"),
                ("completed", stats.completed_count, "Completed", "Awaiting signature"),
                ("signed", stats.signed_count, "Signed", "Attested"),
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
