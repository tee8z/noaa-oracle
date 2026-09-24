use maud::{Markup, html};

use super::weather_map::{region_name, weather_map};

#[derive(Clone)]
pub enum ObservationPeriod {
    Today,
    Selected { start: String, end: String },
}

impl ObservationPeriod {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Today => "Today so far",
            Self::Selected { .. } => "Selected period (UTC)",
        }
    }
}

/// Weather data for display
pub struct WeatherDisplay {
    pub station_id: String,
    pub station_name: String,
    pub state: String,
    pub iata_id: String,
    pub elevation_m: Option<f64>,
    /// Most recent temperature report, independent of the aggregate window.
    pub latest_temp: Option<f64>,
    pub latest_temp_time: Option<String>,
    pub observation_period: ObservationPeriod,
    pub temp_high: Option<f64>,
    pub temp_low: Option<f64>,
    pub wind_speed: Option<i64>,
    pub wind_direction: Option<i64>,
    pub humidity: Option<i64>,
    pub rain_amt: Option<f64>,
    pub snow_amt: Option<f64>,
    pub observed_start: String,
    pub observed_end: String,
    pub latitude: f64,
    pub longitude: f64,
    /// Yesterday's forecast high for today (what was predicted)
    pub forecast_high: Option<i64>,
    /// Yesterday's forecast low for today (what was predicted)
    pub forecast_low: Option<i64>,
}

/// Geographic region based on longitude (matches dashboard.rs get_region)
fn get_region(longitude: f64) -> u8 {
    if longitude < -140.0 {
        0 // Alaska/Hawaii
    } else if longitude < -115.0 {
        1 // Pacific
    } else if longitude < -100.0 {
        2 // Mountain
    } else if longitude < -85.0 {
        3 // Central
    } else {
        4 // Eastern
    }
}

/// Get CSS class for region
fn region_class(region: u8) -> &'static str {
    match region {
        0 => "region-alaska-hawaii",
        1 => "region-pacific",
        2 => "region-mountain",
        3 => "region-central",
        _ => "region-eastern",
    }
}

/// Weather table fragment
/// Shows current weather data for selected stations with map/table toggle
pub fn weather_table(
    weather_data: &[WeatherDisplay],
    all_stations: &[(String, String)],
    refresh_path: &str,
) -> Markup {
    html! {
        div class="box" {
            div class="is-flex is-justify-content-space-between is-align-items-center mb-4 is-flex-wrap-wrap" {
                h2 class="title is-5 mb-0" { "Current Weather" }

                // Station selector dropdown
                div class="dropdown is-hoverable" {
                    div class="dropdown-trigger" {
                        button class="button is-small" aria-haspopup="true" aria-controls="station-menu" {
                            span { "Add Station" }
                            span class="icon is-small" {
                                (chevron_down_icon())
                            }
                        }
                    }
                    div class="dropdown-menu" id="station-menu" role="menu" {
                        div class="dropdown-content station-selector" {
                            @for (station_id, station_name) in all_stations {
                                a class="dropdown-item"
                                  href="#"
                                  hx-get=(format!("/fragments/weather?add_station={}", station_id))
                                  hx-target="#weather-table-container"
                                  hx-swap="outerHTML" {
                                    strong { (station_id) }
                                    " - "
                                    (station_name)
                                }
                            }
                        }
                    }
                }
            }

            // Map/Table toggle tabs
            div class="tabs is-boxed mb-0" {
                ul {
                    li class="is-active" data-view="map" onclick="switchWeatherView('map')" {
                        a {
                            span class="icon is-small" { (map_icon()) }
                            span { "Map" }
                        }
                    }
                    li data-view="table" onclick="switchWeatherView('table')" {
                        a {
                            span class="icon is-small" { (list_icon()) }
                            span { "List" }
                        }
                    }
                }
            }

            (weather_table_body_with_refresh(weather_data, refresh_path))
        }
    }
}

/// Keep request context even when requested stations have no returned readings.
pub fn weather_table_body_with_refresh(
    weather_data: &[WeatherDisplay],
    refresh_url: &str,
) -> Markup {
    html! {
        div id="weather-table-container"
            hx-get=(refresh_url)
            hx-trigger="every 300s"
            hx-swap="outerHTML" {
            @if weather_data.is_empty() {
                div class="has-text-centered has-text-grey py-4" {
                    p { "No weather data available." }
                    p class="is-size-7" { "Weather observations may not be available yet. Try again later." }
                }
            } @else {
                // Map view (default)
                div id="weather-map-view" {
                    (weather_map(weather_data))
                }

                // Table view - desktop only (hidden by default)
                div id="weather-table-view" style="display: none;" {
                    p class="weather-period-note" {
                        strong { (weather_data[0].observation_period.label()) }
                        ". Δ = forecast − observed. "
                        @if matches!(weather_data[0].observation_period, ObservationPeriod::Today) {
                            "Comparisons are preliminary. "
                        }
                        "— = unavailable."
                        @if let ObservationPeriod::Selected { start, end } = &weather_data[0].observation_period {
                            span class="weather-column-context" {
                                time datetime=(start) { (start.replace('T', " ").trim_end_matches('Z')) }
                                " – "
                                time datetime=(end) { (end.replace('T', " ").trim_end_matches('Z')) }
                                " UTC"
                            }
                        }
                    }
                    div class="table-container is-hidden-mobile" {
                        table class="table is-fullwidth is-hoverable weather-observations-table" {
                            thead {
                                tr {
                                    th { "Station" }
                                    th class="has-text-right" { "Latest observed" }
                                    th class="has-text-right" { "Observed high" }
                                    th class="has-text-right" { "Observed low" }
                                    th class="has-text-right" {
                                        @match weather_data[0].observation_period {
                                            ObservationPeriod::Today => { "Yesterday's forecast" },
                                            ObservationPeriod::Selected { .. } => {
                                                "Previous-day forecast"
                                                span class="weather-column-context" { "Issued before selected UTC day" }
                                            },
                                        }
                                        span class="weather-column-context" { "High / low · Δ" }
                                    }
                                    th class="has-text-right" { "Max wind" }
                                    th class="has-text-right" { "Humidity" }
                                    th class="has-text-right" { "Precip" }
                                    th class="has-text-right" { "Snow" }
                                    th { "Observation window" }
                                }
                            }
                            tbody {
                                (render_weather_rows_with_regions(weather_data))
                            }
                        }
                    }

                    // Card view - mobile only
                    div class="weather-cards is-hidden-tablet" {
                        (render_weather_cards_with_regions(weather_data))
                    }
                }
            }
        }
    }
}

/// Render weather rows grouped by region with separator headers
fn render_weather_rows_with_regions(weather_data: &[WeatherDisplay]) -> Markup {
    // Group stations by region
    let mut by_region: Vec<(u8, Vec<&WeatherDisplay>)> = Vec::new();

    for weather in weather_data {
        let region = get_region(weather.longitude);
        if let Some((_, stations)) = by_region.iter_mut().find(|(r, _)| *r == region) {
            stations.push(weather);
        } else {
            by_region.push((region, vec![weather]));
        }
    }

    // Sort regions (they should already be sorted from dashboard, but ensure it)
    by_region.sort_by_key(|(r, _)| *r);

    html! {
        @for (region, stations) in &by_region {
            // Region header row
            tr class={"region-header " (region_class(*region))} {
                td colspan="10" {
                    (region_name(*region))
                }
            }
            // Station rows for this region
            @for weather in stations {
                (render_weather_row(weather))
                // Hidden forecast row
                tr class="forecast-row" id=(format!("forecast-row-{}", weather.station_id)) style="display: none;" {
                    td colspan="10" {
                        div id=(format!("forecast-{}", weather.station_id)) {}
                    }
                }
            }
        }
    }
}

/// Render weather cards grouped by region (mobile view)
fn render_weather_cards_with_regions(weather_data: &[WeatherDisplay]) -> Markup {
    let mut by_region: Vec<(u8, Vec<&WeatherDisplay>)> = Vec::new();

    for weather in weather_data {
        let region = get_region(weather.longitude);
        if let Some((_, stations)) = by_region.iter_mut().find(|(r, _)| *r == region) {
            stations.push(weather);
        } else {
            by_region.push((region, vec![weather]));
        }
    }

    by_region.sort_by_key(|(r, _)| *r);

    html! {
        @for (region, stations) in &by_region {
            div class={"weather-region-header " (region_class(*region))} {
                (region_name(*region))
            }
            @for weather in stations {
                (render_weather_card(weather))
            }
        }
    }
}

/// Render a single weather card (mobile).
fn render_weather_card(weather: &WeatherDisplay) -> Markup {
    html! {
        div class="weather-card box mb-3 is-clickable"
            data-station=(weather.station_id)
            data-forecast-toggle="card" {
            div class="weather-card-header" {
                div {
                    strong { (weather.station_id) }
                    @if !weather.iata_id.is_empty() {
                        " "
                        span class="tag is-iata is-small" { (weather.iata_id) }
                    }
                    (station_description(weather))
                }
            }

            div class="weather-card-latest" {
                span class="weather-card-label" { "Latest observed" }
                (latest_temperature(weather))
            }

            p class="weather-card-period" { (weather.observation_period.label()) }
            table class="weather-temperature-comparison" {
                thead {
                    tr {
                        th {}
                        th scope="col" { "High" }
                        th scope="col" { "Low" }
                    }
                }
                tbody {
                    tr {
                        th scope="row" { "Observed" }
                        td { (temperature_value(weather.temp_high, "temp-high")) }
                        td { (temperature_value(weather.temp_low, "temp-low")) }
                    }
                    tr {
                        th scope="row" {
                            @match weather.observation_period {
                                ObservationPeriod::Today => {
                                    "Forecast"
                                    span class="weather-column-context" { "Issued yesterday" }
                                },
                                ObservationPeriod::Selected { .. } => {
                                    "Previous-day forecast"
                                    span class="weather-column-context" { "Issued before selected UTC day" }
                                },
                            }
                        }
                        td { (temperature_value(weather.forecast_high.map(|t| t as f64), "temp-high")) }
                        td { (temperature_value(weather.forecast_low.map(|t| t as f64), "temp-low")) }
                    }
                    tr {
                        th scope="row" { "Difference (Δ)" }
                        td { (temperature_difference(weather.forecast_high, weather.temp_high)) }
                        td { (temperature_difference(weather.forecast_low, weather.temp_low)) }
                    }
                }
            }

            div class="weather-card-grid" {
                div class="weather-card-item" {
                    span class="weather-card-label" { "Max wind" }
                    (wind_value(weather))
                }
                div class="weather-card-item" {
                    span class="weather-card-label" { "Humidity" }
                    (humidity_value(weather))
                }
                div class="weather-card-item" {
                    span class="weather-card-label" { "Precip" }
                    (precipitation_value(weather.rain_amt, "has-text-info"))
                }
                div class="weather-card-item" {
                    span class="weather-card-label" { "Snow" }
                    (precipitation_value(weather.snow_amt, "has-text-link"))
                }
            }

            div class="weather-observation-window" {
                span class="weather-card-label" { "Observation window" }
                (observation_window(weather))
            }

            button type="button" class="button is-small is-fullwidth weather-forecast-action" {
                "Forecast & history"
                span class="icon is-small" { (chevron_down_icon()) }
            }

            div class="card-forecast"
                id=(format!("card-forecast-{}", weather.station_id))
                style="display: none;" {}
        }
    }
}

/// Render a single weather row.
fn render_weather_row(weather: &WeatherDisplay) -> Markup {
    html! {
        tr class="is-clickable weather-row"
           data-station=(weather.station_id)
           data-forecast-toggle="row" {
            td {
                strong { (weather.station_id) }
                @if !weather.iata_id.is_empty() {
                    " "
                    span class="tag is-iata is-small" { (weather.iata_id) }
                }
                (station_description(weather))
                button type="button" class="weather-forecast-link" { "Forecast & history" }
            }
            td class="has-text-right weather-latest-cell" { (latest_temperature(weather)) }
            td class="has-text-right" { (temperature_value(weather.temp_high, "temp-high")) }
            td class="has-text-right" { (temperature_value(weather.temp_low, "temp-low")) }
            td class="has-text-right weather-forecast-cell" {
                (forecast_comparison("High", weather.forecast_high, weather.temp_high, "temp-high"))
                (forecast_comparison("Low", weather.forecast_low, weather.temp_low, "temp-low"))
            }
            td class="has-text-right" { (wind_value(weather)) }
            td class="has-text-right" { (humidity_value(weather)) }
            td class="has-text-right" { (precipitation_value(weather.rain_amt, "has-text-info")) }
            td class="has-text-right" { (precipitation_value(weather.snow_amt, "has-text-link")) }
            td { (observation_window(weather)) }
        }
    }
}

fn station_description(weather: &WeatherDisplay) -> Markup {
    html! {
        p class="weather-station-description" {
            (weather.station_name)
            @if !weather.station_name.is_empty() && !weather.state.is_empty() {
                ", "
            }
            (weather.state)
            @if let Some(elevation) = weather.elevation_m {
                " "
                span title="Elevation" { (format!("({:.0}m)", elevation)) }
            }
        }
    }
}

fn latest_temperature(weather: &WeatherDisplay) -> Markup {
    html! {
        div class="weather-latest-value" { (temperature_value(weather.latest_temp, "")) }
        @if let Some(timestamp) = &weather.latest_temp_time {
            time class="weather-latest-time local-time" datetime=(timestamp) data-utc=(timestamp) {
                (timestamp)
            }
        }
    }
}

fn temperature_value(value: Option<f64>, class: &str) -> Markup {
    html! {
        @if let Some(temperature) = value {
            @let rounded = temperature.round();
            @let rounded = if rounded == 0.0 { 0.0 } else { rounded };
            span class={ "weather-value " (class) } { (format!("{:.0}°F", rounded)) }
        } @else {
            span class="has-text-grey" { "—" }
        }
    }
}

fn forecast_comparison(
    label: &str,
    forecast: Option<i64>,
    observed: Option<f64>,
    class: &str,
) -> Markup {
    html! {
        div class="weather-forecast-comparison" {
            span class="weather-comparison-label" { (label) }
            (temperature_value(forecast.map(|temperature| temperature as f64), class))
            span class="weather-comparison-difference" {
                "Δ "
                (temperature_difference(forecast, observed))
            }
        }
    }
}

fn temperature_difference(forecast: Option<i64>, observed: Option<f64>) -> Markup {
    html! {
        @if let (Some(forecast), Some(observed)) = (forecast, observed) {
            @let difference = forecast as f64 - observed.round();
            span class={ "weather-value " (accuracy_class(difference)) } {
                (format!("{:+.0}°F", difference))
            }
        } @else {
            span class="has-text-grey" { "—" }
        }
    }
}

fn wind_value(weather: &WeatherDisplay) -> Markup {
    html! {
        @if let Some(wind) = weather.wind_speed {
            span class="weather-value wind" {
                (format!("{} kt", wind))
                @if let Some(direction) = weather.wind_direction {
                    span class="weather-column-context" { (wind_direction_label(direction)) }
                }
            }
        } @else {
            span class="has-text-grey" { "—" }
        }
    }
}

fn humidity_value(weather: &WeatherDisplay) -> Markup {
    html! {
        @if let Some(humidity) = weather.humidity {
            span class="weather-value" { (format!("{}%", humidity)) }
        } @else {
            span class="has-text-grey" { "—" }
        }
    }
}

fn precipitation_value(value: Option<f64>, class: &str) -> Markup {
    html! {
        @if let Some(amount) = value {
            span class={ "weather-value " (class) } { (format!("{:.2}\"", amount)) }
        } @else {
            span class="has-text-grey" { "—" }
        }
    }
}

fn observation_window(weather: &WeatherDisplay) -> Markup {
    html! {
        @if weather.observed_start.is_empty() || weather.observed_end.is_empty() {
            span class="has-text-grey" { "—" }
        } @else {
            span class="is-size-7 local-time-range"
                 data-utc-start=(weather.observed_start)
                 data-utc-end=(weather.observed_end) {
                (weather.observed_start) " – " (weather.observed_end)
            }
        }
    }
}

/// CSS class for forecast accuracy difference
/// Green: within 3°, Yellow: 4-6° off, Red: >6° off
fn accuracy_class(diff: f64) -> &'static str {
    let abs_diff = diff.abs();
    if abs_diff <= 3.0 {
        "has-text-success"
    } else if abs_diff <= 6.0 {
        "has-text-warning"
    } else {
        "has-text-danger"
    }
}

fn wind_direction_label(degrees: i64) -> &'static str {
    match degrees {
        0..=22 | 338..=360 => "N",
        23..=67 => "NE",
        68..=112 => "E",
        113..=157 => "SE",
        158..=202 => "S",
        203..=247 => "SW",
        248..=292 => "W",
        293..=337 => "NW",
        _ => "—",
    }
}

fn chevron_down_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            polyline points="6 9 12 15 18 9" {}
        }
    }
}

fn map_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            polygon points="1 6 1 22 8 18 16 22 23 18 23 2 16 6 8 2 1 6" {}
            line x1="8" y1="2" x2="8" y2="18" {}
            line x1="16" y1="6" x2="16" y2="22" {}
        }
    }
}

fn list_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            line x1="8" y1="6" x2="21" y2="6" {}
            line x1="8" y1="12" x2="21" y2="12" {}
            line x1="8" y1="18" x2="21" y2="18" {}
            line x1="3" y1="6" x2="3.01" y2="6" {}
            line x1="3" y1="12" x2="3.01" y2="12" {}
            line x1="3" y1="18" x2="3.01" y2="18" {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_comparisons_round_half_degrees_like_scoring() {
        for (observed, forecast) in [(-2.5, -3), (54.5, 55)] {
            assert!(
                temperature_value(Some(observed), "")
                    .into_string()
                    .contains(&format!("{forecast}°F"))
            );
            assert!(
                temperature_difference(Some(forecast), Some(observed))
                    .into_string()
                    .contains("+0°F")
            );
        }
        assert!(
            !temperature_value(Some(-0.1), "")
                .into_string()
                .contains("-0°F")
        );
    }
}
