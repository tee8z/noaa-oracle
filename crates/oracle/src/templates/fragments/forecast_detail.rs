use maud::{html, Markup};

/// Forecast data for display
pub struct ForecastDisplay {
    pub date: String,
    pub temp_high: i64,
    pub temp_low: i64,
    pub wind_speed: Option<i64>,
}

/// Forecast detail fragment - shown when a weather row is expanded
pub fn forecast_detail(station_id: &str, forecasts: &[ForecastDisplay]) -> Markup {
    html! {
        div class="forecast-detail p-3" {
            h4 class="title is-6 mb-3" {
                "Forecast for " (station_id)
            }
            @if forecasts.is_empty() {
                p class="has-text-grey" { "No forecast data available." }
            } @else {
                div class="columns is-multiline is-mobile" {
                    @for forecast in forecasts.iter().take(5) {
                        div class="column is-one-fifth-desktop is-half-mobile" {
                            div class="box forecast-day has-text-centered p-2" {
                                p class="is-size-7 has-text-weight-semibold mb-1 local-date" data-utc=(forecast.date.clone()) {
                                    (forecast.date.clone())
                                }
                                p class="mb-1" {
                                    span class="weather-value temp-high" { (format!("{}°", forecast.temp_high)) }
                                    " / "
                                    span class="weather-value temp-low" { (format!("{}°", forecast.temp_low)) }
                                }
                                @if let Some(wind) = forecast.wind_speed {
                                    p class="is-size-7 has-text-grey" {
                                        (format!("{} mph", wind))
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
