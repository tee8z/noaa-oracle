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
                            span class="icon is-small" { (table_icon()) }
                            span { "Table" }
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

            // Table view (hidden by default)
            div id="weather-table-view" style="display: none;" {
                div class="table-container" {
                    table class="table is-fullwidth is-hoverable" {
                        thead {
                            tr {
                                th { "Station" }
                                th class="has-text-right" { "Temp High" }
                                th class="has-text-right" { "Temp Low" }
                                th class="has-text-right" { "Wind (mph)" }
                                th { "Observed" }
                                th { "Updated" }
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
                td colspan="6" {
                    (region_name(*region))
                }
            }
            // Station rows for this region
            @for weather in stations {
                (render_weather_row(weather))
                // Hidden forecast row
                tr class="forecast-row" id=(format!("forecast-row-{}", weather.station_id)) style="display: none;" {
                    td colspan="6" {
                        div id=(format!("forecast-{}", weather.station_id)) {}
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
                        (wind)
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
            td {
                span class="is-size-7 local-time" data-utc=(weather.updated_at.clone()) {
                    (weather.updated_at.clone())
                }
            }
        }
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

fn table_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            rect x="3" y="3" width="18" height="18" rx="2" ry="2" {}
            line x1="3" y1="9" x2="21" y2="9" {}
            line x1="3" y1="15" x2="21" y2="15" {}
            line x1="9" y1="3" x2="9" y2="21" {}
        }
    }
}
