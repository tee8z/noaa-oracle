//! The events list: status filters and an unlisted-event toggle above one
//! page of rows. Rows open the event; the list refreshes itself.

use maud::{Markup, html};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    events::{EventCounts, EventStatus},
    templates::components::time as when,
};

/// Rows on one page of the list.
pub const PAGE_SIZE: usize = 50;

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
    pub unlisted: bool,
}

/// Which events to show. Unlisted events are left out unless asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventFilters {
    pub status: Option<EventStatus>,
    pub show_unlisted: bool,
    /// The page after this event; `None` is the newest page.
    pub before: Option<Uuid>,
}

impl EventFilters {
    pub fn parse(status: Option<&str>, unlisted: Option<&str>, before: Option<&str>) -> Self {
        Self {
            status: status.and_then(parse_status),
            show_unlisted: unlisted == Some("show"),
            before: before.and_then(|id| Uuid::parse_str(id).ok()),
        }
    }

    /// `/events` with these filters.
    pub fn url(&self) -> String {
        let mut parameters = vec![];
        if let Some(status) = self.status {
            parameters.push(format!("status={}", status_param(status)));
        }
        if self.show_unlisted {
            parameters.push("unlisted=show".into());
        }
        if let Some(before) = self.before {
            parameters.push(format!("before={before}"));
        }
        if parameters.is_empty() {
            "/events".into()
        } else {
            format!("/events?{}", parameters.join("&"))
        }
    }

    fn page(self, before: Option<Uuid>) -> Self {
        Self { before, ..self }
    }
}

/// One page of events, the counts for the filters, and whether older
/// events follow.
pub struct EventsPage<'a> {
    pub events: &'a [EventView],
    pub counts: EventCounts,
    pub filters: EventFilters,
    /// The last row's id when an older page exists.
    pub older: Option<Uuid>,
    pub now: OffsetDateTime,
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

/// The filters and the list. Changing a filter replaces this section and
/// returns to the newest page.
pub fn events_section(page: &EventsPage) -> Markup {
    let filters = page.filters;
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
                                " " span class="chip-count" { (page.counts.of(status)) }
                            }
                        }
                    }
                }
                label class="checkbox unlisted-toggle" {
                    input type="checkbox" name="unlisted" value="show"
                        checked[filters.show_unlisted];
                    " Show unlisted"
                    @if !filters.show_unlisted && page.counts.unlisted > 0 {
                        span class="muted" { " (" (page.counts.unlisted) " hidden)" }
                    }
                }
                noscript { button type="submit" class="button is-small" { "Apply" } }
            }
            (events_list(page))
        }
    }
}

/// The rows alone, which refresh every 30 seconds, and the page links.
pub fn events_list(page: &EventsPage) -> Markup {
    let filters = page.filters;
    html! {
        div id="events-list" class="ev-list"
            hx-get=(filters.url()) hx-trigger="every 30s" hx-swap="outerHTML" {
            @if page.events.is_empty() {
                div class="ev-empty" {
                    @if page.counts.of(None) + page.counts.unlisted == 0 {
                        p class="is-size-5" { "No events found" }
                        p class="is-size-7" { "Events will appear here when created by coordinators." }
                    } @else {
                        p { "No events match these filters." }
                    }
                }
            } @else {
                // Visual only: every value carries its own label.
                div class="ev-row ev-header" aria-hidden="true" {
                    span { "Event" }
                    span { "Locations" }
                    span { "Status" }
                    span { "Observation window" }
                    span { "Signing" }
                    span class="ev-num" { "Entries" }
                    span class="ev-num" { "Winning places" }
                }
                @for event in page.events {
                    (event_row(event, page.now))
                }
            }
            @if filters.before.is_some() || page.older.is_some() {
                nav class="ev-pages" aria-label="Pages" {
                    @if filters.before.is_some() {
                        (page_link(filters.page(None), "Newest events"))
                    }
                    @if let Some(older) = page.older {
                        (page_link(filters.page(Some(older)), "Older events"))
                    }
                }
            }
        }
    }
}

fn page_link(filters: EventFilters, label: &str) -> Markup {
    let url = filters.url();
    html! {
        a class="button is-small" href=(url) hx-get=(url) hx-target="#events-list"
          hx-swap="outerHTML" hx-push-url="true" { (label) }
    }
}

fn event_row(event: &EventView, now: OffsetDateTime) -> Markup {
    let href = format!("/events/{}", event.id);
    html! {
        a class="ev-row" href=(href) hx-get=(href) hx-target="#main-content" hx-push-url="true" {
            span class="ev-id" {
                span class="is-sr-only" { "Event " }
                code title=(event.id) { (event.id.get(..8).unwrap_or(&event.id)) }
            }
            span class="ev-locations" {
                span class="is-sr-only" { "Locations " }
                @for location in &event.locations { span class="tag" { (location) } " " }
            }
            span class="ev-status" {
                span class="is-sr-only" { "Status " }
                (status_tag(event.status))
                @if event.unlisted { " " span class="tag is-light" { "Unlisted" } }
            }
            span class="ev-window" {
                span class="cell-label" { "Window: " }
                (when::window(event.start_observation, event.end_observation))
            }
            span class="ev-signing" {
                span class="cell-label" { "Signing: " }
                (when::relative(event.signing_date, now))
            }
            span class="ev-num" {
                span class="cell-label" { "Entries: " }
                (event.total_entries) " / " (event.total_allowed_entries)
            }
            span class="ev-num" {
                span class="cell-label" { "Winning places: " }
                (event.number_of_places_win)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Duration, macros::datetime};

    fn event(id: &str, status: EventStatus, unlisted: bool) -> EventView {
        let start = datetime!(2026-09-24 11:44 UTC);
        EventView {
            id: id.into(),
            locations: vec!["KDEN".into()],
            status,
            start_observation: start,
            end_observation: start + Duration::hours(18),
            signing_date: start + Duration::hours(19),
            total_entries: 3,
            total_allowed_entries: 3,
            number_of_places_win: 1,
            unlisted,
        }
    }

    fn page<'a>(events: &'a [EventView], filters: EventFilters) -> EventsPage<'a> {
        EventsPage {
            events,
            counts: EventCounts {
                running: 1,
                unlisted: 1,
                ..EventCounts::default()
            },
            filters,
            older: None,
            now: datetime!(2026-09-24 12:00 UTC),
        }
    }

    #[test]
    fn the_toggle_says_how_many_unlisted_events_are_hidden() {
        let events = [event("bbbbbbbb-listed", EventStatus::Running, false)];
        let html = events_section(&page(&events, EventFilters::default())).into_string();
        assert!(html.contains("bbbbbbbb"));
        assert!(html.contains("Show unlisted"));
        assert!(html.contains("(1 hidden)"));
        assert!(!html.contains(">Unlisted<"));
        let shown = EventFilters::parse(None, Some("show"), None);
        let html = events_section(&page(&events, shown)).into_string();
        assert!(!html.contains(" hidden)"), "{html}");
    }

    #[test]
    fn unlisted_events_are_tagged_when_shown() {
        let events = [event("aaaaaaaa-unlisted", EventStatus::Signed, true)];
        let shown = EventFilters::parse(None, Some("show"), None);
        let html = events_list(&page(&events, shown)).into_string();
        assert!(html.contains("aaaaaaaa"));
        assert!(html.contains(">Unlisted<"));
    }

    #[test]
    fn filters_round_trip_through_the_url() {
        let before = Uuid::from_u128(7);
        let filters = EventFilters::parse(Some("running"), Some("show"), Some(&before.to_string()));
        assert_eq!(
            filters.url(),
            format!("/events?status=running&unlisted=show&before={before}")
        );
        assert_eq!(
            EventFilters::parse(Some("bogus"), Some("no"), Some("nope")),
            EventFilters::default()
        );
    }

    #[test]
    fn pages_link_to_older_and_back_to_the_newest() {
        let events = [event("bbbbbbbb", EventStatus::Running, false)];
        let older = Uuid::from_u128(9);
        let first = EventsPage {
            older: Some(older),
            ..page(&events, EventFilters::parse(Some("running"), None, None))
        };
        let html = events_list(&first).into_string();
        assert!(
            html.contains(&format!("/events?status=running&amp;before={older}")),
            "{html}"
        );
        assert!(!html.contains("Newest events"));
        assert!(html.contains("Winning places"));
        assert!(!html.contains("Winners"));

        let last = page(
            &events,
            EventFilters::parse(None, None, Some(&older.to_string())),
        );
        let html = events_list(&last).into_string();
        assert!(html.contains("Newest events"));
        assert!(!html.contains("Older events"));
        // The refresh keeps the page.
        assert!(
            html.contains(&format!("hx-get=\"/events?before={older}\"")),
            "{html}"
        );
    }
}
