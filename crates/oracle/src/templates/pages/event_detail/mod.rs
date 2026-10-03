use maud::{Markup, html};
use std::cmp::Reverse;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::events::{Event, EventStatus, Weather, WeatherEntry};
use crate::lines::{Line, LineLevel};
use crate::scoring::{LINE_POINTS, OVER_OR_UNDER_POINTS, PAR_POINTS, ScoringRules};
use crate::sources::Reading;
use crate::templates::{
    components::{time as when, values},
    fragments::events::status_tag,
    layouts::{CurrentPage, PageConfig, base, page_fragment},
};

/// Whether the event's outcome is signed. Why an unsigned event is waiting
/// (a data hold, a retry) is for operators: the API's `settlement_block`
/// and the logs carry it, the public page does not.
#[derive(Clone, Copy)]
pub enum SettlementQuality {
    Signed,
    Unsigned,
}

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
pub fn event_detail_page(event: &Event, now: OffsetDateTime, quality: SettlementQuality) -> Markup {
    let (title, current_page) = config(event);
    base(
        &PageConfig {
            title: &title,
            current_page,
        },
        event_detail_content(event, now, quality),
    )
}

/// What htmx swaps in when an event row is opened.
pub fn event_detail_fragment(
    event: &Event,
    now: OffsetDateTime,
    quality: SettlementQuality,
) -> Markup {
    let (title, current_page) = config(event);
    page_fragment(
        &PageConfig {
            title: &title,
            current_page,
        },
        event_detail_content(event, now, quality),
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

/// The attestation, a scalar, as hex like the API sends it; "Not signed"
/// until the event is signed, as the notice above the grid says. A signed
/// event whose value can't be written out still reads "Signed". An event
/// that reached its signing time without entries is never signed: it has
/// no outcome.
fn attestation_value(event: &Event, now: OffsetDateTime) -> Markup {
    let Some(attestation) = event.attestation.as_ref() else {
        if event.entries.is_empty() && now >= event.signing_date {
            return html! {
                span class="tag" { "Not signed" }
                span class="muted" { " · no entries, so there is no outcome to sign" }
            };
        }
        return html! {
            span class="tag" { "Not signed" }
            @if now < event.signing_date { span class="muted" { " · the signing time has not come" } }
        };
    };
    let hex = serde_json::to_value(attestation)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned));
    match hex {
        Some(hex) => html! { code class="key" { (hex) } },
        None => html! { span class="tag is-signed" { "Signed" } },
    }
}

/// An event settled without entries is never signed, so its readings are not
/// waiting on anything; the notice says so instead of promising a signature.
fn quality_notice(quality: SettlementQuality, settled_without_entries: bool) -> Markup {
    match quality {
        SettlementQuality::Signed => html! {},
        SettlementQuality::Unsigned if settled_without_entries => html! {
            div class="notification" role="status" {
                strong { "Not signed. " }
                "No entries were made, so there is no outcome to sign."
            }
        },
        SettlementQuality::Unsigned => html! {
            div class="notification" role="status" {
                strong { "Not signed. " }
                "Readings and scores are provisional until the oracle signs the outcome."
            }
        },
    }
}

pub fn event_detail_content(
    event: &Event,
    now: OffsetDateTime,
    quality: SettlementQuality,
) -> Markup {
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
                span class="tag" title="Not on the events list; reachable by its link" {
                    "Unlisted"
                }
            }
        }

        (quality_notice(quality, event.settled_without_entries_at.is_some()))

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
                    dt { "Scoring" }
                    dd {
                        @match event.scoring_rules {
                            ScoringRules::Lines => {
                                "Lines: one of Over, Par, and Under happens for each pick; a right pick earns "
                                (LINE_POINTS) " points."
                            }
                            ScoringRules::Fixed => {
                                "Fixed: Par earns " (PAR_POINTS) " points, a right Over or Under "
                                (OVER_OR_UNDER_POINTS) "."
                            }
                        }
                    }
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
                    div { span class="entry-stat" { (event.number_of_places_win) } span class="muted" { "Winning places" } }
                }
            }

            section class="box attestation" {
                h3 class="title is-6" { "Attestation" }
                dl class="facts" {
                    dt { "Outcomes" } dd { (event.event_announcement.locking_points.len()) }
                    dt { "Nonce point" } dd { code class="key" { (event.nonce_point.to_string()) } }
                    dt { "Attestation" }
                    dd { (attestation_value(event, now)) }
                }
            }
        }

        @if !event.weather.is_empty() {
            section class="box" {
                h3 class="title is-6" { "Weather" }
                (weather_comparison_table(&event.weather, provisional(event, now)))
            }
        }

        @if !event.lines.is_empty() {
            section class="box" {
                h3 class="title is-6" { "Par lines" }
                (lines_table(&event.lines, &event.readings))
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

/// A metric's name for readers, as the coordinator names it; an unknown
/// metric shows its key.
fn metric_label(metric: &str) -> &str {
    match metric {
        "temp_high" => "High temperature",
        "temp_low" => "Low temperature",
        "wind_speed" => "Wind speed",
        "wind_direction" => "Wind direction",
        "rain_amt" => "Rain",
        "snow_amt" => "Snow",
        "humidity" => "Humidity",
        other => other,
    }
}

/// A metric's unit after a value.
fn unit(metric: &str) -> &'static str {
    match metric {
        "temp_high" | "temp_low" => "°F",
        "wind_speed" => " kt",
        "humidity" => "%",
        _ => "",
    }
}

/// Each location and metric's line: Par as a miss from the forecast, the
/// same range around the current forecast, and the history it was fitted on.
/// On phones each line is a block of labelled values (`data-label`), so no
/// column is squeezed.
fn lines_table(lines: &[Line], readings: &[Reading]) -> Markup {
    html! {
        p class="muted" {
            "Fixed when the event was created. Par when the observed value minus the forecast "
            "falls inside the band, both ends included; Over above it, Under below. Each band "
            "was fitted so that over its past windows the three came out about equally often."
        }
        div class="table-container" {
            table class="table is-fullwidth is-narrow lines-table" {
                thead {
                    tr {
                        th { "Location" }
                        th { "Metric" }
                        th { "Par (observed − forecast)" }
                        th { "Par now" }
                        th { "Fitted on" }
                    }
                }
                tbody {
                    @for line in lines {
                        @let unit = unit(&line.metric);
                        @let forecast = readings
                            .iter()
                            .find(|reading| reading.target == line.target && reading.metric == line.metric)
                            .and_then(|reading| reading.baseline);
                        tr {
                            td data-label="Location" { (line.target) }
                            td data-label="Metric" { (metric_label(&line.metric)) }
                            td data-label="Par" {
                                (format!("{:+.1} to {:+.1}{unit}", line.lower, line.upper))
                            }
                            td data-label="Par now" {
                                @match forecast {
                                    Some(forecast) => {
                                        (format!("{:.1}–{:.1}{unit}", forecast + line.lower, forecast + line.upper))
                                    }
                                    None => { span class="muted" { "no forecast yet" } }
                                }
                            }
                            td class="muted" data-label="Fitted on" {
                                (line.windows) " windows"
                                @if line.level == LineLevel::Pooled { ", all stations" }
                                " · Over " (line.over) ", Par " (line.par) ", Under " (line.under)
                            }
                        }
                    }
                }
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

/// Why the differences stay grey, if they do. The forecasts are for whole
/// days: until the window closes the observed high and low can still move,
/// and a window shorter than a day holds only some of a day's reports.
fn provisional(event: &Event, now: OffsetDateTime) -> Option<&'static str> {
    if now < event.end_observation_date {
        Some("The window is still open, so differences stay grey.")
    } else if event.end_observation_date - event.start_observation_date < time::Duration::days(1) {
        Some(
            "The window is shorter than a day, so differences stay grey: its reports can't be judged against a whole day's forecast.",
        )
    } else {
        None
    }
}

/// Forecast baseline against what was observed during the window.
fn weather_comparison_table(weather: &[Weather], provisional: Option<&str>) -> Markup {
    let settled = if provisional.is_some() {
        values::Settled::SoFar
    } else {
        values::Settled::Final
    };
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
            @if let Some(reason) = provisional {
                " " (reason)
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

/// Explain the all-entry outcome without confusing it with ranked ties, and
/// without a word about money: that is the competition's business. Missing
/// observations are only one cause.
fn no_score_reason(nothing_observed: bool, window: time::Duration) -> String {
    let why = if nothing_observed {
        format!(
            "No hourly station report fell inside the {} observation window, so no entry scored.",
            duration(window)
        )
    } else {
        "No entry scored any points.".into()
    };
    format!("No-score outcome. {why} The oracle signed the outcome for all entries.")
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
        &no_score_reason(nothing_observed, window),
    )
}

fn entries_list(
    entries: &[WeatherEntry],
    num_winners: usize,
    signed: bool,
    no_score: &str,
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
                @if signed { (no_score) } @else { "No entry has scored yet." }
            }
        }
        div class="table-container" {
            table class="table is-fullwidth is-narrow" {
                thead {
                    tr {
                        th { "Rank" }
                        th { "Entry ID" }
                        th class="has-text-right" { "Points" }
                    }
                }
                tbody {
                    @for (idx, entry) in ranked.iter().enumerate() {
                        @let paid = show_ranks && idx < num_winners;
                        tr class=[paid.then_some("is-paid")] {
                            td {
                                @if no_points && signed {
                                    span class="tag" title="No entry scored any points; the oracle signed the outcome for all entries" { "No score" }
                                } @else if !show_ranks {
                                    span class="muted" { "—" }
                                } @else if paid {
                                    span class="tag is-success" title="Winning place" { (format!("#{}", idx + 1)) }
                                } @else {
                                    span class="muted" { (format!("#{}", idx + 1)) }
                                }
                            }
                            td {
                                code class="entry-id" { (entry.id.to_string()) }
                            }
                            td class="has-text-right" {
                                @if no_points && signed {
                                    span class="muted" { "—" }
                                } @else if let Some(points) = points(entry) {
                                    span class=(if paid { "entry-score paid" } else { "entry-score" }) {
                                        (points)
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

/// An entry's points. Events scored before
/// points were stored show the stored score.
fn points(entry: &WeatherEntry) -> Option<String> {
    match (entry.base_score, entry.score) {
        (Some(points), _) => Some(format!("{points} pts")),
        (None, Some(score)) => Some(score.to_string()),
        (None, None) => None,
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
    use dlctix::secp::MaybeScalar;
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
        let html = entries_list(&entries, 1, true, "No-score outcome.").into_string();
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
        let html = entries_list(&entries, 1, true, "No-score outcome.").into_string();
        assert_eq!(displayed_ids(&html), vec![entries[1].id, entries[0].id]);
    }

    #[test]
    fn unscored_and_split_entries_do_not_get_arbitrary_winner_badges() {
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
                "No-score outcome.",
            ),
        ] {
            let html = entries_list(&entries, 1, signed, "No-score outcome.").into_string();
            assert!(html.contains(message));
            assert!(!html.contains("entry-score paid"));
            assert!(!html.contains("is-paid"));
            assert!(!html.contains("#1"));
        }
    }

    #[test]
    fn lines_show_par_around_the_forecast_and_their_history() {
        let now = time::macros::datetime!(2026-09-27 00:00 UTC);
        let line = |target: &str, level| Line {
            target: target.into(),
            metric: "temp_high".into(),
            lower: -1.6,
            upper: 1.2,
            level,
            window_hours: 24,
            windows: 58,
            over: 19,
            par: 20,
            under: 19,
            first_window: now - time::Duration::days(60),
            last_window: now - time::Duration::days(1),
            fitted_at: now,
        };
        let readings = [Reading {
            target: "KDEN".into(),
            metric: "temp_high".into(),
            baseline: Some(70.0),
            observed: None,
        }];
        let html = lines_table(
            &[
                line("KDEN", LineLevel::Station),
                line("KBJC", LineLevel::Pooled),
            ],
            &readings,
        )
        .into_string();
        assert!(html.contains("-1.6 to +1.2°F"), "{html}");
        assert!(html.contains("68.4–71.2°F"), "{html}");
        assert!(html.contains("no forecast yet"));
        assert_eq!(html.matches("all stations").count(), 1);
        assert!(html.contains("Over 19, Par 20, Under 19"));
        assert!(html.contains(">High temperature</td>"), "{html}");
        assert!(!html.contains("temp_high"), "{html}");
        assert_eq!(html.matches("data-label=\"Fitted on\"").count(), 2);
    }

    #[test]
    fn metrics_have_reader_names() {
        assert_eq!(metric_label("temp_low"), "Low temperature");
        assert_eq!(metric_label("wind_speed"), "Wind speed");
        assert_eq!(metric_label("new_metric"), "new_metric");
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
        let running =
            weather_comparison_table(&weather, Some("The window is still open.")).into_string();
        assert!(running.contains("is-provisional"), "{running}");
        assert!(!running.contains("is-far"), "{running}");
        assert!(running.contains("still open"));
        let ended = weather_comparison_table(&weather, None).into_string();
        assert!(ended.contains("is-far"), "{ended}");
        assert!(!ended.contains("is-provisional"));
    }

    fn rfc3339(time: OffsetDateTime) -> String {
        time.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    fn event(
        window: time::Duration,
        entries: Vec<WeatherEntry>,
        attestation: Option<MaybeScalar>,
    ) -> Event {
        let start = time::macros::datetime!(2026-09-25 16:46:51 UTC);
        let mut event: Event = serde_json::from_value(serde_json::json!({
            "id": Uuid::now_v7(),
            "signing_date": rfc3339(start + window + time::Duration::minutes(5)),
            "start_observation_date": rfc3339(start),
            "end_observation_date": rfc3339(start + window),
            "locations": ["KDEN"],
            "number_of_values_per_entry": 3,
            "status": "Completed",
            "total_allowed_entries": 3,
            "entry_ids": [],
            "number_of_places_win": 1,
            "entries": [],
            "source": "noaa_weather",
            "readings": [],
            "weather": [],
            "nonce_point": "03d0d3a122dab2922858fd9e61c2745723fabcdbb397cc077edc51e0b11073163f",
            "event_announcement": {"locking_points": [], "expiry": 1_790_442_111},
            "attestation": null,
            "coordinator_pubkey": "npub1test",
            "scoring_fields": ["temp_high"],
        }))
        .unwrap_or_else(|error| panic!("test event: {error}"));
        event.entries = entries;
        event.attestation = attestation;
        event
    }

    #[test]
    fn the_notice_promises_a_signature_only_for_events_that_can_get_one() {
        let waiting = quality_notice(SettlementQuality::Unsigned, false).into_string();
        assert!(
            waiting.contains("provisional until the oracle signs"),
            "{waiting}"
        );

        let empty = quality_notice(SettlementQuality::Unsigned, true).into_string();
        assert!(empty.contains("Not signed."), "{empty}");
        assert!(empty.contains("no outcome to sign"), "{empty}");
        assert!(!empty.contains("provisional"), "{empty}");

        assert!(
            quality_notice(SettlementQuality::Signed, true)
                .into_string()
                .is_empty()
        );
    }

    #[test]
    fn the_attestation_is_hex_pending_or_not_signed_without_entries() {
        let window = time::Duration::minutes(10);
        let attestation = MaybeScalar::from_slice(&[7; 32]).unwrap();
        let entries = vec![entry(1, None, None)];
        let signed = event(window, entries.clone(), Some(attestation));
        let later = signed.signing_date + time::Duration::hours(5);
        let html = attestation_value(&signed, later).into_string();
        assert!(html.contains(&"07".repeat(32)), "{html}");
        assert!(!html.contains("Pending"), "{html}");

        let waiting = event(window, entries, None);
        assert!(
            attestation_value(&waiting, later)
                .into_string()
                .contains("Not signed")
        );

        let empty = event(window, vec![], None);
        let before = empty.signing_date - time::Duration::minutes(1);
        assert!(
            attestation_value(&empty, before)
                .into_string()
                .contains("Not signed")
        );
        let html = attestation_value(&empty, later).into_string();
        assert!(html.contains("Not signed"), "{html}");
        assert!(html.contains("no entries"), "{html}");
        assert!(!html.contains("Pending"), "{html}");
    }

    #[test]
    fn an_unsigned_event_says_so_without_a_data_warning() {
        let mut pending = event(time::Duration::days(1), vec![], None);
        pending.weather.push(Weather {
            station_id: "KPWM".into(),
            observed: Some(Observed {
                date: pending.start_observation_date,
                temp_high: 140,
                temp_low: 41,
                wind_speed: None,
            }),
            forecasted: Forecasted {
                date: pending.start_observation_date,
                temp_high: 66,
                temp_low: 42,
                wind_speed: None,
            },
        });
        let html =
            event_detail_content(&pending, pending.signing_date, SettlementQuality::Unsigned)
                .into_string();
        let notice = html.find("role=\"status\"").expect("visible status notice");
        let weather = html.find(">Weather</h3>").expect("stored weather retained");
        assert!(notice < weather, "{html}");
        assert!(html.contains("Not signed."), "{html}");
        for warning in ["role=\"alert\"", "blocked", "review", "verif", "reports"] {
            assert!(!html.contains(warning), "{warning}: {html}");
        }

        let signed =
            event_detail_content(&pending, pending.signing_date, SettlementQuality::Signed)
                .into_string();
        assert!(!signed.contains("role=\"status\""), "{signed}");
        assert!(!signed.contains("Not signed."), "{signed}");
    }

    #[test]
    fn short_windows_never_settle_against_whole_day_forecasts() {
        let short = event(time::Duration::minutes(10), vec![], None);
        let later = short.signing_date + time::Duration::hours(5);
        assert!(
            provisional(&short, later)
                .unwrap()
                .contains("shorter than a day")
        );
        let day = event(time::Duration::days(1), vec![], None);
        assert!(
            provisional(&day, day.start_observation_date)
                .unwrap()
                .contains("still open")
        );
        assert_eq!(provisional(&day, day.end_observation_date), None);
    }

    #[test]
    fn no_score_outcomes_explain_observations_without_claiming_payment() {
        let short = time::Duration::minutes(10);
        let reason = no_score_reason(true, short);
        assert!(
            reason.contains("No hourly station report fell inside the 10 min observation window")
        );
        assert!(reason.starts_with("No-score outcome."), "{reason}");
        assert!(reason.contains("outcome for all entries"), "{reason}");
        for money in ["refund", "pot", "ties", "pay", "Pay", "contract"] {
            assert!(!reason.contains(money), "{money}: {reason}");
        }
        let with_readings = no_score_reason(false, time::Duration::hours(18));
        assert!(with_readings.contains("No entry scored any points."));
        assert!(!with_readings.contains("No hourly station report"));
        assert_eq!(duration(time::Duration::minutes(90)), "1 h 30 min");

        let entries = [
            entry(1, Some(10_000), Some(0)),
            entry(2, Some(10_000), Some(0)),
        ];
        let html = entries_list(&entries, 1, true, &reason).into_string();
        assert_eq!(html.matches(">No score<").count(), 2, "{html}");
        assert!(!html.contains("pts"), "{html}");
        assert!(!html.contains("10000"), "{html}");
    }
}
