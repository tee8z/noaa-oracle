use maud::{html, Markup};

/// Event statistics data
#[derive(Default)]
pub struct EventStats {
    pub live_count: usize,
    pub running_count: usize,
    pub completed_count: usize,
    pub signed_count: usize,
}

/// Event statistics display fragment
/// Shows counts of events by status in a responsive grid
pub fn event_stats(stats: &EventStats) -> Markup {
    html! {
        div class="box" {
            h2 class="title is-5 mb-4" { "Event Statistics" }

            div class="columns is-multiline is-mobile" {
                // Live events
                div class="column is-half-mobile is-one-quarter-tablet" {
                    div class="stat-card" {
                        div class="stat-value has-text-success" {
                            (stats.live_count)
                        }
                        div class="stat-label" { "Live" }
                        p class="is-size-7 has-text-grey" { "Accepting entries" }
                    }
                }

                // Running events
                div class="column is-half-mobile is-one-quarter-tablet" {
                    div class="stat-card" {
                        div class="stat-value has-text-warning-dark" {
                            (stats.running_count)
                        }
                        div class="stat-label" { "Running" }
                        p class="is-size-7 has-text-grey" { "Observing weather" }
                    }
                }

                // Completed events
                div class="column is-half-mobile is-one-quarter-tablet" {
                    div class="stat-card" {
                        div class="stat-value has-text-info" {
                            (stats.completed_count)
                        }
                        div class="stat-label" { "Completed" }
                        p class="is-size-7 has-text-grey" { "Awaiting signature" }
                    }
                }

                // Signed events
                div class="column is-half-mobile is-one-quarter-tablet" {
                    div class="stat-card" {
                        div class="stat-value has-text-primary" {
                            (stats.signed_count)
                        }
                        div class="stat-label" { "Signed" }
                        p class="is-size-7 has-text-grey" { "Attested" }
                    }
                }
            }
        }
    }
}
