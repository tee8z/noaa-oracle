use maud::{html, Markup};

/// Weather data for display
pub struct WeatherDisplay {
    pub station_id: String,
    pub station_name: String,
    pub temp_high: Option<f64>,
    pub temp_low: Option<f64>,
    pub wind_speed: Option<i64>,
    pub last_updated: String,
}

/// Weather table fragment
/// Shows current weather data for selected stations
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
                p { "No active events with weather stations." }
                p class="is-size-7" { "Weather data will appear when events are created." }
            }
        } @else {
            div class="table-container" {
                table class="table is-fullwidth is-striped is-hoverable" {
                    thead {
                        tr {
                            th { "Station" }
                            th class="has-text-right" { "Temp High" }
                            th class="has-text-right" { "Temp Low" }
                            th class="has-text-right" { "Wind (mph)" }
                            th { "Updated" }
                        }
                    }
                    tbody hx-get="/fragments/weather"
                          hx-trigger="every 1s"
                          hx-swap="innerHTML"
                          hx-select="tbody > tr" {
                        @for weather in weather_data {
                            tr {
                                td {
                                    strong { (weather.station_id.clone()) }
                                    @if !weather.station_name.is_empty() {
                                        br;
                                        span class="is-size-7 has-text-grey" {
                                            (weather.station_name.clone())
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
                                    span class="is-size-7" {
                                        (weather.last_updated.clone())
                                    }
                                }
                            }
                        }
                    }
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
