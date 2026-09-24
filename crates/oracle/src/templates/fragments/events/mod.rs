//! The events list: status filters and a test-event toggle above one row
//! per event. Rows open the event; the list refreshes itself.

use maud::{Markup, html};
use time::{Duration, OffsetDateTime};

use crate::{events::EventStatus, templates::components::time as when};

/// Windows shorter than this are test runs (synthetic checks run every
/// hour with ten-minute windows).
const TEST_WINDOW: Duration = Duration::hours(1);

/// Event view data for the list
pub struct EventView {
    pub id: String,
    pub locations: Vec<String>,
    pub status: EventStatus,
    pub start_observation: OffsetDateTime,
    pub end_observation: OffsetDateTime,
    pub signing_date: OffsetDateTime,
    pub total_entries: i64,
    pub total_allowed_entries: i64,
    pub number_of_places_win: i64,
}

impl EventView {
    pub fn is_test(&self) -> bool {
        self.end_observation - self.start_observation < TEST_WINDOW
    }
}

/// Which events to show. Test events are hidden unless asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventFilters {
    pub status: Option<EventStatus>,
    pub show_tests: bool,
}

impl EventFilters {
    pub fn parse(status: Option<&str>, tests: Option<&str>) -> Self {
        Self {
            status: status.and_then(parse_status),
            show_tests: tests == Some("show"),
        }
    }

    /// `/events` with these filters.
    pub fn url(&self) -> String {
        let mut parameters = vec![];
        if let Some(status) = self.status {
            parameters.push(format!("status={}", status_param(status)));
        }
        if self.show_tests {
            parameters.push("tests=show".into());
        }
        if parameters.is_empty() {
            "/events".into()
        } else {
            format!("/events?{}", parameters.join("&"))
        }
    }

    fn shows(&self, event: &EventView) -> bool {
        (self.show_tests || !event.is_test()) && self.status.is_none_or(|status| event.status == status)
    }
}

const STATUSES: [EventStatus; 4] = [
    EventStatus::Live,
    EventStatus::Running,
    EventStatus::Completed,
    EventStatus::Signed,
];

fn parse_status(value: &str) -> Option<EventStatus> {
    STATUSES
        .into_iter()
        .find(|status| status_param(*status) == value)
}

fn status_param(status: EventStatus) -> &'static str {
    match status {
        EventStatus::Live => "live",
        EventStatus::Running => "running",
        EventStatus::Completed => "completed",
        EventStatus::Signed => "signed",
    }
}

pub fn status_text(status: EventStatus) -> &'static str {
    match status {
        EventStatus::Live => "Live",
        EventStatus::Running => "Running",
        EventStatus::Completed => "Completed",
        EventStatus::Signed => "Signed",
    }
}

pub fn status_tag(status: EventStatus) -> Markup {
    html! { span class={ "tag is-" (status_param(status)) } { (status_text(status)) } }
}

/// The filters and the list. Changing a filter replaces this section.
pub fn events_section(events: &[EventView], filters: EventFilters, now: OffsetDateTime) -> Markup {
    let candidates: Vec<_> = events
        .iter()
        .filter(|event| filters.show_tests || !event.is_test())
        .collect();
    let hidden_tests = events.iter().filter(|event| event.is_test()).count();
    let count = |status: Option<EventStatus>| {
        candidates
            .iter()
            .filter(|event| status.is_none_or(|status| event.status == status))
            .count()
    };
    html! {
        section id="events" class="box" {
            h2 class="title is-5" { "Oracle events" }
            form class="event-filters" action="/events" method="get"
                hx-get="/events" hx-target="#events" hx-swap="outerHTML"
                hx-trigger="change" hx-push-url="true" {
                fieldset class="status-filter" {
                    legend class="is-sr-only" { "Status" }
                    @for status in std::iter::once(None).chain(STATUSES.into_iter().map(Some)) {
                        label class="status-chip" {
                            input type="radio" name="status"
                                value=(status.map_or("", status_param))
                                checked[filters.status == status];
                            span {
                                (status.map_or("All", status_text))
                                " " span class="chip-count" { (count(status)) }
                            }
                        }
                    }
                }
                label class="checkbox test-toggle" {
                    input type="checkbox" name="tests" value="show" checked[filters.show_tests];
                    " Show test events"
                    span class="muted" {
                        " (windows under an hour"
                        @if !filters.show_tests && hidden_tests > 0 { ", " (hidden_tests) " hidden" }
                        ")"
                    }
                }
                noscript { button type="submit" class="button is-small" { "Apply" } }
            }
            (events_list(events, filters, now))
        }
    }
}

/// The rows alone, which refresh every 30 seconds.
pub fn events_list(events: &[EventView], filters: EventFilters, now: OffsetDateTime) -> Markup {
    let shown: Vec<_> = events.iter().filter(|event| filters.shows(event)).collect();
    html! {
        div id="events-list" class="ev-list"
            hx-get=(filters.url()) hx-trigger="every 30s" hx-swap="outerHTML" {
            @if shown.is_empty() {
                div class="ev-empty" {
                    @if events.is_empty() {
                        p class="is-size-5" { "No events found" }
                        p class="is-size-7" { "Events will appear here when created by coordinators." }
                    } @else {
                        p { "No events match these filters." }
                    }
                }
            } @else {
                div class="ev-row ev-header" aria-hidden="true" {
                    span { "Event" }
                    span { "Locations" }
                    span { "Status" }
                    span { "Observation window" }
                    span { "Signing" }
                    span class="ev-num" { "Entries" }
                    span class="ev-num" { "Paid places" }
                }
                @for event in shown {
                    (event_row(event, now))
                }
            }
        }
    }
}

fn event_row(event: &EventView, now: OffsetDateTime) -> Markup {
    let href = format!("/events/{}", event.id);
    html! {
        a class="ev-row" href=(href) hx-get=(href) hx-target="#main-content" hx-push-url="true" {
            span class="ev-id" { code title=(event.id) { (event.id.get(..8).unwrap_or(&event.id)) } }
            span class="ev-locations" {
                @for location in &event.locations { span class="tag" { (location) } " " }
            }
            span class="ev-status" { (status_tag(event.status)) }
            span class="ev-window" data-label="Window" {
                (when::window(event.start_observation, event.end_observation))
            }
            span class="ev-signing" data-label="Signing" { (when::relative(event.signing_date, now)) }
            span class="ev-num" data-label="Entries" { (event.total_entries) " / " (event.total_allowed_entries) }
            span class="ev-num" data-label="Paid places" { (event.number_of_places_win) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn event(id: &str, status: EventStatus, minutes: i64) -> EventView {
        let start = datetime!(2026-09-24 11:44 UTC);
        EventView {
            id: id.into(),
            locations: vec!["KDEN".into()],
            status,
            start_observation: start,
            end_observation: start + Duration::minutes(minutes),
            signing_date: start + Duration::minutes(minutes + 5),
            total_entries: 3,
            total_allowed_entries: 3,
            number_of_places_win: 1,
        }
    }

    #[test]
    fn test_events_are_hidden_unless_asked_for() {
        let events = [
            event("aaaaaaaa-test", EventStatus::Signed, 10),
            event("bbbbbbbb-real", EventStatus::Running, 18 * 60),
        ];
        let now = datetime!(2026-09-24 12:00 UTC);
        let html = events_section(&events, EventFilters::default(), now).into_string();
        assert!(!html.contains("aaaaaaaa"));
        assert!(html.contains("bbbbbbbb"));
        assert!(html.contains("1 hidden"));
        let html = events_list(&events, EventFilters::parse(None, Some("show")), now).into_string();
        assert!(html.contains("aaaaaaaa"));
    }

    #[test]
    fn status_filter_keeps_only_that_status() {
        let events = [
            event("aaaaaaaa", EventStatus::Signed, 120),
            event("bbbbbbbb", EventStatus::Running, 120),
        ];
        let filters = EventFilters::parse(Some("running"), None);
        assert_eq!(filters.url(), "/events?status=running");
        let html = events_list(&events, filters, datetime!(2026-09-24 12:00 UTC)).into_string();
        assert!(!html.contains("aaaaaaaa"));
        assert!(html.contains("bbbbbbbb"));
        assert!(html.contains("Paid places"));
        assert!(!html.contains("Winners"));
        assert_eq!(EventFilters::parse(Some("bogus"), Some("no")), EventFilters::default());
    }
}
