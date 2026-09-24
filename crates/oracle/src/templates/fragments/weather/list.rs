//! The station list: one `<details>` per station, grouped by region under a
//! sticky header. Opening a station fetches its forecast and history once.

use maud::{Markup, html};

use super::{ObservationPeriod, WeatherContext, WeatherDisplay, by_region, place, with_parameters};
use crate::templates::components::{time as when, values};

/// At most this many stations outside the list are offered for a search.
const OTHER_MATCHES: usize = 8;

/// The search box. Typing replaces only `#weather-list`, so the box keeps
/// focus; the server puts the search in the address bar.
pub(super) fn search_form(context: &WeatherContext) -> Markup {
    let action = with_parameters(context.selection_path, &[("view", "list")]);
    html! {
        form class="weather-search" role="search"
            hx-get=(action)
            hx-target="#weather-list"
            hx-sync="this:replace"
            hx-swap="outerHTML"
            hx-trigger="input changed delay:200ms from:#weather-search, search from:#weather-search, submit"
            hx-indicator="#weather-search-loading" {
            label class="is-sr-only" for="weather-search" { "Search stations" }
            input id="weather-search" class="input is-small" type="search" name="q"
                value=(context.query) autocomplete="off"
                placeholder="Search by code, city or state";
            span id="weather-search-loading" class="loader htmx-indicator" aria-hidden="true" {}
        }
    }
}

/// Whether a station matches the search: code, airport code, name or state.
fn matches(query: &str, id: &str, iata: &str, name: &str, state: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let contains = |field: &str| field.to_lowercase().contains(&query);
    contains(id) || contains(iata) || contains(name) || state.eq_ignore_ascii_case(&query)
}

pub fn weather_list(weather: &[WeatherDisplay], context: &WeatherContext) -> Markup {
    let shown: Vec<_> = weather
        .iter()
        .filter(|w| {
            matches(
                context.query,
                &w.station_id,
                &w.iata_id,
                &w.station_name,
                &w.state,
            )
        })
        .collect();
    let others: Vec<_> = if context.query.trim().len() < 2 {
        vec![]
    } else {
        context
            .stations
            .iter()
            .filter(|station| !weather.iter().any(|w| w.station_id == station.station_id))
            .filter(|s| {
                matches(
                    context.query,
                    &s.station_id,
                    &s.iata_id,
                    &s.station_name,
                    &s.state,
                )
            })
            .take(OTHER_MATCHES)
            .collect()
    };
    let forecast_label = match weather.first().map(|w| &w.observation_period) {
        Some(ObservationPeriod::Selected { .. }) => "Forecast from the day before",
        _ => "Yesterday's forecast",
    };
    html! {
        div id="weather-list" class="wx-list" {
            @if weather.is_empty() {
                (no_data())
            } @else if shown.is_empty() {
                p class="wx-empty" { "No station in this list matches “" (context.query.trim()) "”." }
            } @else {
                div class="wx-row wx-header" aria-hidden="true" {
                    span class="wx-name" { "Station" }
                    span class="wx-latest" { "Latest" }
                    span class="wx-high" { "High" }
                    span class="wx-low" { "Low" }
                    span class="wx-fcst" { (forecast_label) }
                    span class="wx-wind" { "Max wind" }
                    span class="wx-humidity" { "Humidity" }
                    span class="wx-rain" { "Precip" }
                    span class="wx-snow" { "Snow" }
                }
                @for (region, stations) in by_region(shown) {
                    details class="wx-region" open {
                        summary {
                            (region.name())
                            span class="wx-count" { (stations.len()) }
                        }
                        @for station in stations {
                            (station_row(station, context))
                        }
                    }
                }
            }
            @if !others.is_empty() {
                div class="wx-others" {
                    p { "Not in this list:" }
                    ul {
                        @for station in others {
                            li {
                                a href=(super::page_url(&with_parameters(context.selection_path, &[("add_station", &station.station_id), ("view", "list")])))
                                  hx-get=(with_parameters(context.selection_path, &[("add_station", &station.station_id), ("view", "list")]))
                                  hx-target="#weather-table-container"
                                  hx-swap="outerHTML" {
                                    "Add " strong { (station.station_id) } " "
                                    (station.station_name)
                                    @if !station.state.is_empty() { ", " (station.state) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub(super) fn no_data() -> Markup {
    html! {
        div class="wx-empty" {
            p { "No weather data available." }
            p class="is-size-7" { "Weather observations may not be available yet. Try again later." }
        }
    }
}

fn station_row(weather: &WeatherDisplay, context: &WeatherContext) -> Markup {
    let settled = weather.observation_period.settled(context.now);
    let forecast_high = weather.forecast_high.map(|t| t as f64);
    let forecast_low = weather.forecast_low.map(|t| t as f64);
    let observed = match (
        when::parse(&weather.observed_start),
        when::parse(&weather.observed_end),
    ) {
        (Some(start), Some(end)) => format!("Observed {}", when::window_text(start, end)),
        _ => "No observations in this period".into(),
    };
    html! {
        details class="wx-station"
            hx-get=(format!("/fragments/forecast/{}", weather.station_id))
            hx-trigger="toggle once"
            hx-swap="innerHTML"
            hx-target="find .wx-forecast" {
            summary class="wx-row" title=(observed) {
                span class="wx-name" {
                    strong { (weather.station_id) }
                    @if !weather.iata_id.is_empty() {
                        " " span class="tag is-iata" { (weather.iata_id) }
                    }
                    span class="wx-place" { (place(weather)) }
                }
                span class="wx-latest" data-label="Latest" {
                    (values::temperature(weather.latest_temp, ""))
                    @if let Some(time) = weather.latest_temp_time.as_deref().and_then(when::parse) {
                        " " span class="wx-when" { (when::relative(time, context.now)) }
                    }
                }
                span class="wx-high" data-label="High" {
                    (values::temperature(weather.temp_high, "temp-high"))
                    " " (values::difference(weather.temp_high, forecast_high, "°F", settled))
                }
                span class="wx-low" data-label="Low" {
                    (values::temperature(weather.temp_low, "temp-low"))
                    " " (values::difference(weather.temp_low, forecast_low, "°F", settled))
                }
                span class="wx-fcst" data-label="Forecast" {
                    @if forecast_high.is_none() && forecast_low.is_none() {
                        (values::missing())
                    } @else {
                        (values::temperature(forecast_high, "temp-high"))
                        " / "
                        (values::temperature(forecast_low, "temp-low"))
                    }
                }
                span class="wx-wind" data-label="Wind" { (values::wind(weather.wind_speed, weather.wind_direction)) }
                span class="wx-humidity" data-label="Humidity" { (values::percent(weather.humidity)) }
                span class="wx-rain" data-label="Precip" { (values::precipitation(weather.rain_amt, "rain", 2)) }
                span class="wx-snow" data-label="Snow" { (values::precipitation(weather.snow_amt, "snow", 2)) }
            }
            div class="wx-forecast"
                data-load-error="Couldn't load the forecast and history." {
                p class="wx-loading" { span class="loader" {} " Loading forecast and history…" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_codes_names_and_states() {
        let hit = |q| matches(q, "KORD", "ORD", "Chicago O'Hare", "IL");
        assert!(hit(""));
        assert!(hit("kord"));
        assert!(hit("ord"));
        assert!(hit("chicago"));
        assert!(hit("IL"));
        assert!(!hit("xyz"));
        assert!(!hit("boston"));
    }
}
