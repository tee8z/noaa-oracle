use dlctix::secp::MaybeScalar;
use maud::{Markup, html};
use std::cmp::Reverse;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::events::{Event, EventStatus, Weather, WeatherEntry};
use crate::templates::{
    components::{time as when, values},
    fragments::events::status_tag,
    layouts::{CurrentPage, PageConfig, base, page_fragment},
};

fn config(event: &Event) -> (String, CurrentPage) {
    (
        format!(
            "Event {} - 4cast Truth Oracle",
            truncate_id(&event.id.to_string())
        ),
        CurrentPage::Events,
    )
}

/// Event detail page - shows full information about a single event
pub fn event_detail_page(event: &Event, now: OffsetDateTime) -> Markup {
    let (title, current_page) = config(event);
    base(
        &PageConfig {
            title: &title,
            current_page,
        },
        event_detail_content(event, now),
    )
}

/// What htmx swaps in when an event row is opened.
pub fn event_detail_fragment(event: &Event, now: OffsetDateTime) -> Markup {
    let (title, current_page) = config(event);
    page_fragment(
        &PageConfig {
            title: &title,
            current_page,
        },
        event_detail_content(event, now),
    )
}

const NOT_FOUND: PageConfig<'static> = PageConfig {
    title: "Event not found - 4cast Truth Oracle",
    current_page: CurrentPage::Events,
};

pub fn event_not_found_page(event_id: Uuid) -> Markup {
    base(&NOT_FOUND, event_not_found_content(event_id))
}

pub fn event_not_found_fragment(event_id: Uuid) -> Markup {
    page_fragment(&NOT_FOUND, event_not_found_content(event_id))
}

fn event_not_found_content(event_id: Uuid) -> Markup {
    event_problem(
        "Event not found",
        html! { "No event has the ID " code { (event_id) } "." },
    )
}

const UNAVAILABLE: PageConfig<'static> = PageConfig {
    title: "Event unavailable - 4cast Truth Oracle",
    current_page: CurrentPage::Events,
};

/// The event could not be read. htmx 4 swaps error replies too, so this
/// replaces the page's content instead of leaving it blank.
pub fn event_unavailable_page(event_id: Uuid) -> Markup {
    base(&UNAVAILABLE, event_unavailable_content(event_id))
}

pub fn event_unavailable_fragment(event_id: Uuid) -> Markup {
    page_fragment(&UNAVAILABLE, event_unavailable_content(event_id))
}

fn event_unavailable_content(event_id: Uuid) -> Markup {
    event_problem(
        "Event unavailable",
        html! { "Event " code { (event_id) } " could not be read. Try again in a moment." },
    )
}

fn event_problem(heading: &str, message: Markup) -> Markup {
    html! {
        section class="box" {
            h2 class="title is-5" { (heading) }
            p { (message) }
            a href="/events" class="button is-small mt-3"
              hx-get="/events" hx-target="#main-content" hx-push-url="true" {
                "All events"
            }
        }
    }
}

/// The attestation, a scalar, as hex like the API sends it; "Pending"
/// until the event is signed. A signed event whose value can't be written
/// out still reads "Signed", never "Pending".
fn attestation_value(attestation: Option<&MaybeScalar>) -> Markup {
    let Some(attestation) = attestation else {
        return html! { span class="tag is-warning is-light" { "Pending" } };
    };
    let hex = serde_json::to_value(attestation)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned));
    match hex {
        Some(hex) => html! { code class="key" { (hex) } },
        None => html! { span class="tag is-signed" { "Signed" } },
    }
}

pub fn event_detail_content(event: &Event, now: OffsetDateTime) -> Markup {
    let window = event.end_observation_date - event.start_observation_date;
    html! {
        div class="event-detail-header" {
            a href="/events" class="button is-small back-btn"
               hx-get="/events"
               hx-target="#main-content"
               hx-push-url="true" {
                span class="icon" { (back_icon()) }
                span { "Events" }
            }
            h2 class="title is-4 mb-0" { "Event " code { (truncate_id(&event.id.to_string())) } }
            (status_tag(event.status))
            @if event.unlisted {
                span class="tag is-light" title="Not on the events list; reachable by its link" {
                    "Unlisted"
                }
            }
        }

        div class="event-grid" {
            section class="box" {
                h3 class="title is-6" { "Event" }
                dl class="facts" {
                    dt { "Event ID" } dd { code { (event.id.to_string()) } }
                    dt { "Locations" }
                    dd {
                        @for location in &event.locations { span class="tag" { (location) } " " }
                    }
                    dt { "Values per entry" } dd { (event.number_of_values_per_entry) }
                    dt { "Coordinator" } dd { code class="key" { (event.coordinator_pubkey) } }
                }
            }

            section class="box" {
                h3 class="title is-6" { "Timeline" }
                dl class="facts" {
                    dt { "Observation window" }
                    dd {
                        (when::window(event.start_observation_date, event.end_observation_date))
                        span class="muted" { " (" (duration(window)) ")" }
                    }
                    dt { "Signing" }
                    dd {
                        (when::absolute(event.signing_date))
                        span class="muted" { " · " (when::ago(event.signing_date, now)) }
                    }
                }
            }

            section class="box" {
                h3 class="title is-6" { "Entries" }
                div class="entry-stats" {
                    div { span class="entry-stat" { (event.entries.len()) } span class="muted" { "Entries" } }
                    div { span class="entry-stat" { (event.total_allowed_entries) } span class="muted" { "Allowed" } }
                    div { span class="entry-stat" { (event.number_of_places_win) } span class="muted" { "Paid places" } }
                }
            }

            section class="box" {
                h3 class="title is-6" { "Attestation" }
                dl class="facts" {
                    dt { "Outcomes" } dd { (event.event_announcement.locking_points.len()) }
                    dt { "Nonce point" } dd { code class="key" { (event.nonce_point.to_string()) } }
                    dt { "Attestation" }
                    dd { (attestation_value(event.attestation.as_ref())) }
                }
            }
        }

        @if !event.weather.is_empty() {
            section class="box" {
                h3 class="title is-6" { "Weather" }
                (weather_comparison_table(&event.weather, settled(event, now)))
            }
        }

        @if !event.entries.is_empty() && event.status != EventStatus::Live {
            section class="box" {
                h3 class="title is-6" { "Entries" }
                (entries_table(event, event.number_of_places_win as usize, event.status == EventStatus::Signed))
            }
        }
    }
}

/// "10 min", "18 h", "2 days"
fn duration(span: time::Duration) -> String {
    if span < time::Duration::hours(1) {
        format!("{} min", span.whole_minutes())
    } else if span < time::Duration::hours(48) {
        let hours = span.whole_hours();
        let minutes = span.whole_minutes() % 60;
        if minutes == 0 {
            format!("{hours} h")
        } else {
            format!("{hours} h {minutes} min")
        }
    } else {
        format!("{} days", span.whole_days())
    }
}

/// Differences stay provisional until the observation window closes: until
/// then the observed high and low can still move.
fn settled(event: &Event, now: OffsetDateTime) -> values::Settled {
    if now < event.end_observation_date {
        values::Settled::SoFar
    } else {
        values::Settled::Final
    }
}

/// Forecast baseline against what was observed during the window.
fn weather_comparison_table(weather: &[Weather], settled: values::Settled) -> Markup {
    html! {
        div class="table-container" {
            table class="table is-fullwidth is-narrow event-weather" {
                thead {
                    tr {
                        th { "Station" }
                        th { "High" }
                        th { "Low" }
                        th { "Max wind" }
                    }
                }
                tbody {
                    @for w in weather {
                        @let observed = w.observed.as_ref();
                        tr {
                            th scope="row" { (w.station_id) }
                            td { (observed_and_forecast(observed.map(|o| o.temp_high as f64), Some(w.forecasted.temp_high as f64), "°F", "temp-high", settled)) }
                            td { (observed_and_forecast(observed.map(|o| o.temp_low as f64), Some(w.forecasted.temp_low as f64), "°F", "temp-low", settled)) }
                            td { (observed_and_forecast(observed.and_then(|o| o.wind_speed).map(|v| v as f64), w.forecasted.wind_speed.map(|v| v as f64), " kt", "", settled)) }
                        }
                    }
                }
            }
        }
        p class="is-size-7 muted" {
            "Each cell: observed and observed − forecast, then " span class="fcst" { "forecast" } ". — means no report in the window."
            @if settled == values::Settled::SoFar {
                " The window is still open, so differences stay grey."
            }
        }
    }
}

fn observed_and_forecast(
    observed: Option<f64>,
    forecast: Option<f64>,
    unit: &str,
    class: &str,
    settled: values::Settled,
) -> Markup {
    let show = |value: Option<f64>| match value {
        Some(value) => html! { span class={ "val " (class) } { (format!("{value:.0}{unit}")) } },
        None => values::missing(),
    };
    html! {
        span class="obs" {
            (show(observed))
            @if observed.is_some() && forecast.is_some() {
                " " (values::difference(observed, forecast, unit, settled))
            }
        }
        span class="fcst" { (show(forecast)) }
    }
}

/// Why every entry was refunded, when the event says so.
fn refund_reason(nothing_observed: bool, window: time::Duration) -> String {
    if nothing_observed {
        format!(
            "All entries refunded: no hourly station report fell inside the {} observation window, so there was nothing to score.",
            duration(window)
        )
    } else {
        "All entries refunded: no entry scored any points.".into()
    }
}

fn entries_table(event: &Event, num_winners: usize, signed: bool) -> Markup {
    let nothing_observed = event
        .readings
        .iter()
        .all(|reading| reading.observed.is_none())
        && event
            .weather
            .iter()
            .all(|weather| weather.observed.is_none());
    let window = event.end_observation_date - event.start_observation_date;
    entries_list(
        &event.entries,
        num_winners,
        signed,
        &refund_reason(nothing_observed, window),
    )
}

fn entries_list(
    entries: &[WeatherEntry],
    num_winners: usize,
    signed: bool,
    refunded: &str,
) -> Markup {
    // API entries stay in id order because outcome indices depend on it.
    // Sort references for display only. Stored scores also preserve the
    // ranking of historical events signed with the older score formula.
    let mut ranked: Vec<_> = entries.iter().collect();
    ranked.sort_unstable_by_key(|entry| (Reverse(entry.score), entry.id));
    let scores_ready = !ranked.is_empty() && ranked.iter().all(|entry| entry.score.is_some());
    let no_points = !ranked.is_empty() && ranked.iter().all(|entry| entry.base_score == Some(0));
    let show_ranks = scores_ready && !no_points;
    html! {
        @if !scores_ready {
            p class="entries-note" { "Scores pending." }
        } @else if no_points {
            p class="entries-note" {
                @if signed { (refunded) } @else { "No entry has scored yet." }
            }
        }
        div class="table-container" {
            table class="table is-fullwidth is-narrow" {
                thead {
                    tr {
                        th { "Rank" }
                        th { "Entry ID" }
                        th class="has-text-right" { "Score" }
                    }
                }
                tbody {
                    @for (idx, entry) in ranked.iter().enumerate() {
                        @let paid = show_ranks && idx < num_winners;
                        tr class=[paid.then_some("is-paid")] {
                            td {
                                @if !show_ranks {
                                    span class="muted" { "—" }
                                } @else if paid {
                                    span class="tag is-success" title="Paid place" { (format!("#{}", idx + 1)) }
                                } @else {
                                    span class="muted" { (format!("#{}", idx + 1)) }
                                }
                            }
                            td {
                                code class="entry-id" { (entry.id.to_string()) }
                            }
                            td class="has-text-right" {
                                @if let Some(score) = entry.score {
                                    span class=(if paid { "entry-score paid" } else { "entry-score" }) {
                                        (score)
                                    }
                                } @else {
                                    span class="muted" { "—" }
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

fn back_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            line x1="19" y1="12" x2="5" y2="12" {}
            polyline points="12 19 5 12 12 5" {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Forecasted, Observed};
    use uuid::Uuid;

    fn entry(id: u128, score: Option<i64>, base_score: Option<i64>) -> WeatherEntry {
        WeatherEntry {
            id: Uuid::from_u128(id),
            event_id: Uuid::nil(),
            picks: vec![],
            expected_observations: vec![],
            score,
            base_score,
        }
    }

    fn displayed_ids(html: &str) -> Vec<Uuid> {
        html.split("<code class=\"entry-id\">")
            .skip(1)
            .map(|cell| Uuid::parse_str(cell.split_once("</code>").unwrap().0).unwrap())
            .collect()
    }

    #[test]
    fn entry_ranks_follow_stored_scores_then_ids_without_reordering_api_entries() {
        let entries = vec![
            entry(1, Some(100_000), Some(10)),
            entry(3, Some(200_000), Some(20)),
            entry(2, Some(200_000), Some(20)),
        ];
        let original = entries.clone();
        let html = entries_list(&entries, 1, true, "All entries refunded.").into_string();
        assert_eq!(
            displayed_ids(&html),
            vec![entries[2].id, entries[1].id, entries[0].id]
        );
        assert_eq!(entries, original);
        assert_eq!(html.matches("entry-score paid").count(), 1);
        assert!(html.contains("#1"));
    }

    #[test]
    fn historical_signed_scores_keep_their_original_ranking() {
        let entries = [
            entry(1, Some(190_001), Some(20)),
            entry(2, Some(200_000), Some(20)),
        ];
        let html = entries_list(&entries, 1, true, "All entries refunded.").into_string();
        assert_eq!(displayed_ids(&html), vec![entries[1].id, entries[0].id]);
    }

    #[test]
    fn unscored_and_refunded_entries_do_not_get_arbitrary_winner_badges() {
        for (entries, signed, message) in [
            (
                vec![entry(1, None, None), entry(2, None, None)],
                false,
                "Scores pending.",
            ),
            (
                vec![
                    entry(1, Some(10_000), Some(0)),
                    entry(2, Some(10_000), Some(0)),
                ],
                false,
                "No entry has scored yet.",
            ),
            (
                vec![
                    entry(1, Some(10_000), Some(0)),
                    entry(2, Some(10_000), Some(0)),
                ],
                true,
                "All entries refunded.",
            ),
        ] {
            let html = entries_list(&entries, 1, signed, "All entries refunded.").into_string();
            assert!(html.contains(message));
            assert!(!html.contains("entry-score paid"));
            assert!(!html.contains("is-paid"));
            assert!(!html.contains("#1"));
        }
    }

    #[test]
    fn running_events_show_provisional_differences() {
        let weather = [Weather {
            station_id: "KDEN".into(),
            observed: Some(Observed {
                date: time::macros::datetime!(2026-09-24 00:00 UTC),
                temp_low: 40,
                temp_high: 52,
                wind_speed: None,
            }),
            forecasted: Forecasted {
                date: time::macros::datetime!(2026-09-24 00:00 UTC),
                temp_low: 45,
                temp_high: 70,
                wind_speed: None,
            },
        }];
        let running = weather_comparison_table(&weather, values::Settled::SoFar).into_string();
        assert!(running.contains("is-provisional"), "{running}");
        assert!(!running.contains("is-far"), "{running}");
        assert!(running.contains("still open"));
        let ended = weather_comparison_table(&weather, values::Settled::Final).into_string();
        assert!(ended.contains("is-far"), "{ended}");
        assert!(!ended.contains("is-provisional"));
    }

    #[test]
    fn a_signed_event_shows_its_attestation_as_hex() {
        let attestation = MaybeScalar::from_slice(&[7; 32]).unwrap();
        let html = attestation_value(Some(&attestation)).into_string();
        assert!(html.contains(&"07".repeat(32)), "{html}");
        assert!(!html.contains("Pending"), "{html}");
        assert!(attestation_value(None).into_string().contains("Pending"));
    }

    #[test]
    fn refunds_say_why() {
        let short = time::Duration::minutes(10);
        let reason = refund_reason(true, short);
        assert!(reason.starts_with("All entries refunded"));
        assert!(
            reason.contains("no hourly station report fell inside the 10 min observation window")
        );
        assert_eq!(
            refund_reason(false, time::Duration::hours(18)),
            "All entries refunded: no entry scored any points."
        );
        assert_eq!(duration(time::Duration::minutes(90)), "1 h 30 min");
    }
}
