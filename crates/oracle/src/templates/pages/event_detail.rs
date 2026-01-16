use maud::{html, Markup};

use crate::db::{Event, EventStatus, Weather, WeatherEntry};
use crate::templates::layouts::{base, CurrentPage, PageConfig};

/// Event detail page - shows full information about a single event
pub fn event_detail_page(api_base: &str, event: &Event) -> Markup {
    let config = PageConfig {
        title: &format!(
            "Event {} - 4cast Truth Oracle",
            truncate_id(&event.id.to_string())
        ),
        api_base,
        current_page: CurrentPage::Events,
    };

    base(&config, event_detail_content(event))
}

/// Event detail content - can be used for full page or HTMX partial
pub fn event_detail_content(event: &Event) -> Markup {
    html! {
        // Back button and header
        div class="event-detail-header" {
            a href="/events" class="button is-light back-btn"
               hx-get="/events"
               hx-target="#main-content"
               hx-push-url="true" {
                span class="icon" { (back_icon()) }
                span { "Back to Events" }
            }

            h2 class="title is-4 mb-0" {
                "Event Details"
            }

            span class=(status_class(&event.status)) {
                (status_text(&event.status))
            }
        }

        // Event overview
        div class="columns is-multiline" {
            // Basic info card
            div class="column is-full-mobile is-half-desktop" {
                div class="box" {
                    h3 class="title is-6 mb-3" { "Event Information" }

                    table class="table is-fullwidth is-narrow" {
                        tbody {
                            tr {
                                th { "Event ID" }
                                td {
                                    code class="is-size-7" { (event.id.to_string()) }
                                }
                            }
                            tr {
                                th { "Coordinator" }
                                td {
                                    code class="is-size-7 truncate" style="max-width: 200px; display: inline-block;" {
                                        (event.coordinator_pubkey.clone())
                                    }
                                }
                            }
                            tr {
                                th { "Locations" }
                                td {
                                    div class="tags are-small" {
                                        @for location in &event.locations {
                                            span class="tag is-light" { (location) }
                                        }
                                    }
                                }
                            }
                            tr {
                                th { "Values per Entry" }
                                td { (event.number_of_values_per_entry) }
                            }
                        }
                    }
                }
            }

            // Dates card
            div class="column is-full-mobile is-half-desktop" {
                div class="box" {
                    h3 class="title is-6 mb-3" { "Timeline" }

                    table class="table is-fullwidth is-narrow" {
                        tbody {
                            tr {
                                th { "Observation Start" }
                                td {
                                    span class="local-time" data-utc=(format_datetime(&event.start_observation_date)) {
                                        (format_datetime(&event.start_observation_date))
                                    }
                                }
                            }
                            tr {
                                th { "Observation End" }
                                td {
                                    span class="local-time" data-utc=(format_datetime(&event.end_observation_date)) {
                                        (format_datetime(&event.end_observation_date))
                                    }
                                }
                            }
                            tr {
                                th { "Signing Date" }
                                td {
                                    span class="local-time" data-utc=(format_datetime(&event.signing_date)) {
                                        (format_datetime(&event.signing_date))
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Entry stats card
            div class="column is-full-mobile is-half-desktop" {
                div class="box" {
                    h3 class="title is-6 mb-3" { "Entry Statistics" }

                    div class="columns is-mobile has-text-centered" {
                        div class="column" {
                            p class="is-size-3 has-text-weight-bold" {
                                (event.entries.len())
                            }
                            p class="is-size-7 has-text-grey" { "Current Entries" }
                        }
                        div class="column" {
                            p class="is-size-3 has-text-weight-bold" {
                                (event.total_allowed_entries)
                            }
                            p class="is-size-7 has-text-grey" { "Max Entries" }
                        }
                        div class="column" {
                            p class="is-size-3 has-text-weight-bold has-text-success" {
                                (event.number_of_places_win)
                            }
                            p class="is-size-7 has-text-grey" { "Winners" }
                        }
                    }
                }
            }

            // DLC info card
            div class="column is-full-mobile is-half-desktop" {
                div class="box" {
                    h3 class="title is-6 mb-3" { "DLC Information" }

                    div class="mb-3" {
                        p class="is-size-7 has-text-grey mb-1" { "Nonce" }
                        div class="dlc-info" {
                            (format!("{:?}", event.nonce))
                        }
                    }

                    div class="mb-3" {
                        p class="is-size-7 has-text-grey mb-1" { "Locking Points" }
                        p { (event.event_announcement.locking_points.len()) " outcomes" }
                    }

                    @if let Some(ref attestation) = event.attestation {
                        div {
                            p class="is-size-7 has-text-grey mb-1" { "Attestation" }
                            div class="dlc-info" {
                                (format!("{:?}", attestation))
                            }
                        }
                    } @else {
                        div {
                            p class="is-size-7 has-text-grey mb-1" { "Attestation" }
                            span class="tag is-warning is-light" { "Pending" }
                        }
                    }
                }
            }
        }

        // Weather data section
        @if !event.weather.is_empty() {
            div class="box mt-4" {
                h3 class="title is-6 mb-3" { "Weather Data" }
                (weather_comparison_table(&event.weather))
            }
        }

        // Entries section (only show if event is running or completed)
        @if !event.entries.is_empty() && event.status != EventStatus::Live {
            div class="box mt-4" {
                h3 class="title is-6 mb-3" { "Entries" }
                (entries_table(&event.entries, event.number_of_places_win as usize))
            }
        }
    }
}

fn weather_comparison_table(weather: &[Weather]) -> Markup {
    html! {
        div class="table-container" {
            table class="table is-fullwidth is-striped weather-comparison" {
                thead {
                    tr {
                        th { "Station" }
                        th colspan="2" class="has-text-centered" { "Temp High" }
                        th colspan="2" class="has-text-centered" { "Temp Low" }
                        th colspan="2" class="has-text-centered" { "Wind Speed" }
                    }
                    tr {
                        th {}
                        th class="is-size-7" { "Forecast" }
                        th class="is-size-7" { "Observed" }
                        th class="is-size-7" { "Forecast" }
                        th class="is-size-7" { "Observed" }
                        th class="is-size-7" { "Forecast" }
                        th class="is-size-7" { "Observed" }
                    }
                }
                tbody {
                    @for w in weather {
                        tr {
                            td { strong { (w.station_id.clone()) } }
                            // Temp high
                            td class="forecast-value" {
                                (format!("{}°F", w.forecasted.temp_high))
                            }
                            td class="observed-value" {
                                @if let Some(ref obs) = w.observed {
                                    (format!("{:.0}°F", obs.temp_high))
                                } @else {
                                    span class="has-text-grey" { "-" }
                                }
                            }
                            // Temp low
                            td class="forecast-value" {
                                (format!("{}°F", w.forecasted.temp_low))
                            }
                            td class="observed-value" {
                                @if let Some(ref obs) = w.observed {
                                    (format!("{:.0}°F", obs.temp_low))
                                } @else {
                                    span class="has-text-grey" { "-" }
                                }
                            }
                            // Wind speed
                            td class="forecast-value" {
                                @if let Some(wind) = w.forecasted.wind_speed {
                                    (format!("{} mph", wind))
                                } @else {
                                    span class="has-text-grey" { "-" }
                                }
                            }
                            td class="observed-value" {
                                @if let Some(ref obs) = w.observed {
                                    (format!("{} mph", obs.wind_speed))
                                } @else {
                                    span class="has-text-grey" { "-" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn entries_table(entries: &[WeatherEntry], num_winners: usize) -> Markup {
    html! {
        div class="table-container" {
            table class="table is-fullwidth is-striped" {
                thead {
                    tr {
                        th { "Rank" }
                        th { "Entry ID" }
                        th class="has-text-right" { "Score" }
                    }
                }
                tbody {
                    @for (idx, entry) in entries.iter().enumerate() {
                        tr class=(if idx < num_winners { "has-background-success-light" } else { "" }) {
                            td {
                                @if idx < num_winners {
                                    span class="tag is-success" { (format!("#{}", idx + 1)) }
                                } @else {
                                    span class="has-text-grey" { (format!("#{}", idx + 1)) }
                                }
                            }
                            td {
                                code class="is-size-7" { (entry.id.to_string()) }
                            }
                            td class="has-text-right" {
                                @if let Some(score) = entry.score {
                                    span class=(if idx < num_winners { "entry-score winner" } else { "entry-score" }) {
                                        (score)
                                    }
                                } @else {
                                    span class="has-text-grey" { "-" }
                                }
                            }
                        }
                    }
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

fn format_datetime(dt: &time::OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "Invalid date".to_string())
}

fn status_class(status: &EventStatus) -> &'static str {
    match status {
        EventStatus::Live => "tag is-live is-medium ml-3",
        EventStatus::Running => "tag is-running is-medium ml-3",
        EventStatus::Completed => "tag is-completed is-medium ml-3",
        EventStatus::Signed => "tag is-signed is-medium ml-3",
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

fn back_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            line x1="19" y1="12" x2="5" y2="12" {}
            polyline points="12 19 5 12 12 5" {}
        }
    }
}
