use maud::{html, Markup};

use super::weather_map::{region_name, weather_map};

/// Weather data for display
pub struct WeatherDisplay {
    pub station_id: String,
    pub station_name: String,
    pub state: String,
    pub iata_id: String,
    pub elevation_m: Option<f64>,
    pub temp_high: Option<f64>,
    pub temp_low: Option<f64>,
    pub wind_speed: Option<i64>,
    pub wind_direction: Option<i64>,
    pub humidity: Option<i64>,
    pub rain_amt: Option<f64>,
    pub snow_amt: Option<f64>,
    pub observed_start: String,
    pub observed_end: String,
    pub updated_at: String,
    pub latitude: f64,
    pub longitude: f64,
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
pub fn weather_table(weather_data: &[WeatherDisplay], all_stations: &[(String, String)]) -> Markup {
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
                                  hx-swap="innerHTML" {
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

            div id="weather-table-container" {
                (weather_table_body(weather_data))
            }
        }
    }
}

/// Just the table body - used for HTMX partial updates
pub fn weather_table_body(weather_data: &[WeatherDisplay]) -> Markup {
    html! {
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
                div class="table-container is-hidden-mobile" {
                    table class="table is-fullwidth is-hoverable" {
                        thead {
                            tr {
                                th { "Station" }
                                th class="has-text-right" { "Temp High" }
                                th class="has-text-right" { "Temp Low" }
                                th class="has-text-right" { "Wind" }
                                th class="has-text-right" { "Humidity" }
                                th class="has-text-right" { "Precip" }
                                th class="has-text-right" { "Snow" }
                                th { "Observed" }
                            }
                        }
                        tbody hx-get="/fragments/weather"
                              hx-trigger="every 300s"
                              hx-swap="innerHTML"
                              hx-select="tbody > tr" {
                            // Group by region and render with separators
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
                td colspan="8" {
                    (region_name(*region))
                }
            }
            // Station rows for this region
            @for weather in stations {
                (render_weather_row(weather))
                // Hidden forecast row
                tr class="forecast-row" id=(format!("forecast-row-{}", weather.station_id)) style="display: none;" {
                    td colspan="8" {
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

/// Render a single weather card (mobile)
fn render_weather_card(weather: &WeatherDisplay) -> Markup {
    html! {
        div class="weather-card box mb-3"
            data-station=(weather.station_id.clone()) {
            // Header: station ID + name
            div class="is-flex is-justify-content-space-between is-align-items-center mb-2" {
                div {
                    strong { (weather.station_id.clone()) }
                    @if !weather.iata_id.is_empty() {
                        " "
                        span class="tag is-iata is-small" { (weather.iata_id.clone()) }
                    }
                }
            }
            @if !weather.station_name.is_empty() || !weather.state.is_empty() {
                p class="is-size-7 has-text-grey mb-2" {
                    @if !weather.station_name.is_empty() {
                        (weather.station_name.clone())
                    }
                    @if !weather.station_name.is_empty() && !weather.state.is_empty() {
                        ", "
                    }
                    @if !weather.state.is_empty() {
                        (weather.state.clone())
                    }
                    @if let Some(elev) = weather.elevation_m {
                        " "
                        span class="has-text-grey-light" { (format!("({:.0}m)", elev)) }
                    }
                }
            }

            // Weather values in a grid
            div class="weather-card-grid" {
                div class="weather-card-item" {
                    span class="weather-card-label" { "High" }
                    @if let Some(temp) = weather.temp_high {
                        span class="weather-value temp-high" { (format!("{:.0}°F", temp)) }
                    } @else {
                        span class="has-text-grey" { "-" }
                    }
                }
                div class="weather-card-item" {
                    span class="weather-card-label" { "Low" }
                    @if let Some(temp) = weather.temp_low {
                        span class="weather-value temp-low" { (format!("{:.0}°F", temp)) }
                    } @else {
                        span class="has-text-grey" { "-" }
                    }
                }
                div class="weather-card-item" {
                    span class="weather-card-label" { "Wind" }
                    @if let Some(wind) = weather.wind_speed {
                        span class="weather-value wind" {
                            (format!("{}", wind))
                            @if let Some(dir) = weather.wind_direction {
                                span class="has-text-grey is-size-7" { (format!(" {}", wind_direction_label(dir))) }
                            }
                        }
                    } @else {
                        span class="has-text-grey" { "-" }
                    }
                }
                div class="weather-card-item" {
                    span class="weather-card-label" { "Humidity" }
                    @if let Some(humidity) = weather.humidity {
                        span class="weather-value" { (format!("{}%", humidity)) }
                    } @else {
                        span class="has-text-grey" { "-" }
                    }
                }
                div class="weather-card-item" {
                    span class="weather-card-label" { "Precip" }
                    @if let Some(rain) = weather.rain_amt {
                        @if rain > 0.0 {
                            span class="weather-value has-text-info" { (format!("{:.2}\"", rain)) }
                        } @else {
                            span class="has-text-grey" { "-" }
                        }
                    } @else {
                        span class="has-text-grey" { "-" }
                    }
                }
                div class="weather-card-item" {
                    span class="weather-card-label" { "Snow" }
                    @if let Some(snow) = weather.snow_amt {
                        @if snow > 0.0 {
                            span class="weather-value has-text-link" { (format!("{:.1}\"", snow)) }
                        } @else {
                            span class="has-text-grey" { "-" }
                        }
                    } @else {
                        span class="has-text-grey" { "-" }
                    }
                }
            }

            // Observed time
            div class="mt-2 pt-2" style="border-top: 1px solid var(--bulma-border);" {
                span class="is-size-7 has-text-grey" {
                    "Observed: "
                    span class="local-time-range"
                         data-utc-start=(weather.observed_start.clone())
                         data-utc-end=(weather.observed_end.clone()) {
                        (weather.observed_start.clone()) " - " (weather.observed_end.clone())
                    }
                }
            }
        }
    }
}

/// Render a single weather row
fn render_weather_row(weather: &WeatherDisplay) -> Markup {
    html! {
        tr class="is-clickable weather-row"
           data-station=(weather.station_id.clone())
           onclick=(format!("loadForecast('{}')", weather.station_id)) {
            td {
                strong { (weather.station_id.clone()) }
                @if !weather.iata_id.is_empty() {
                    " "
                    span class="tag is-iata is-small" { (weather.iata_id.clone()) }
                }
                br;
                span class="is-size-7 has-text-grey" {
                    @if !weather.station_name.is_empty() {
                        (weather.station_name.clone())
                    }
                    @if !weather.station_name.is_empty() && !weather.state.is_empty() {
                        ", "
                    }
                    @if !weather.state.is_empty() {
                        (weather.state.clone())
                    }
                    @if let Some(elev) = weather.elevation_m {
                        " "
                        span class="has-text-grey-light" title="Elevation" {
                            (format!("({:.0}m)", elev))
                        }
                    }
                }
            }
            td class="has-text-right" {
                @if let Some(temp) = weather.temp_high {
                    span class="weather-value temp-high" {
                        (format!("{:.0}°F", temp))
                    }
                } @else {
                    span class="has-text-grey" { "-" }
                }
            }
            td class="has-text-right" {
                @if let Some(temp) = weather.temp_low {
                    span class="weather-value temp-low" {
                        (format!("{:.0}°F", temp))
                    }
                } @else {
                    span class="has-text-grey" { "-" }
                }
            }
            td class="has-text-right" {
                @if let Some(wind) = weather.wind_speed {
                    span class="weather-value wind" {
                        (format!("{}", wind))
                        @if let Some(dir) = weather.wind_direction {
                            span class="has-text-grey is-size-7" { (format!(" {}", wind_direction_label(dir))) }
                        }
                    }
                } @else {
                    span class="has-text-grey" { "-" }
                }
            }
            td class="has-text-right" {
                @if let Some(humidity) = weather.humidity {
                    span class="weather-value" {
                        (format!("{}%", humidity))
                    }
                } @else {
                    span class="has-text-grey" { "-" }
                }
            }
            td class="has-text-right" {
                @if let Some(rain) = weather.rain_amt {
                    @if rain > 0.0 {
                        span class="weather-value has-text-info" {
                            (format!("{:.2}\"", rain))
                        }
                    } @else {
                        span class="has-text-grey" { "-" }
                    }
                } @else {
                    span class="has-text-grey" { "-" }
                }
            }
            td class="has-text-right" {
                @if let Some(snow) = weather.snow_amt {
                    @if snow > 0.0 {
                        span class="weather-value has-text-link" {
                            (format!("{:.1}\"", snow))
                        }
                    } @else {
                        span class="has-text-grey" { "-" }
                    }
                } @else {
                    span class="has-text-grey" { "-" }
                }
            }
            td {
                span class="is-size-7 local-time-range"
                     data-utc-start=(weather.observed_start.clone())
                     data-utc-end=(weather.observed_end.clone()) {
                    (weather.observed_start.clone()) " - " (weather.observed_end.clone())
                }
            }
        }
    }
}

/// Convert wind direction degrees to compass label
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
        _ => "",
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
