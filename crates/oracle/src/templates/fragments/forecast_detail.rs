use maud::{html, Markup};

/// Forecast data for display
pub struct ForecastDisplay {
    pub date: String,
    pub temp_high: i64,
    pub temp_low: i64,
    pub wind_speed: Option<i64>,
    pub precip_chance: Option<i64>,
}

/// Comparison of forecast vs actual observation for a past day
pub struct ForecastComparison {
    pub date: String,
    pub forecast_high: i64,
    pub forecast_low: i64,
    pub actual_high: Option<f64>,
    pub actual_low: Option<f64>,
}

/// Forecast detail fragment - shown when a weather row is expanded
pub fn forecast_detail(
    _station_id: &str,
    comparisons: &[ForecastComparison],
    forecasts: &[ForecastDisplay],
) -> Markup {
    html! {
        div class="forecast-detail p-3" {
            // Recent accuracy section
            @if !comparisons.is_empty() {
                h4 class="title is-6 mb-3" {
                    "Recent Accuracy"
                }
                div class="columns is-multiline is-mobile mb-4" {
                    @for comp in comparisons.iter().take(3) {
                        div class="column is-one-third-desktop is-full-mobile" {
                            div class="box forecast-day p-2" {
                                p class="is-size-7 has-text-weight-semibold mb-2 local-date" data-utc=(comp.date.clone()) {
                                    (comp.date.clone())
                                }
                                div class="is-flex is-justify-content-space-between" {
                                    div class="has-text-centered" {
                                        p class="is-size-7 has-text-grey" { "Forecast" }
                                        p {
                                            span class="weather-value temp-high" { (format!("{}°", comp.forecast_high)) }
                                            " / "
                                            span class="weather-value temp-low" { (format!("{}°", comp.forecast_low)) }
                                        }
                                    }
                                    div class="has-text-centered" {
                                        p class="is-size-7 has-text-grey" { "Actual" }
                                        @if let (Some(high), Some(low)) = (comp.actual_high, comp.actual_low) {
                                            p {
                                                span class="weather-value temp-high" { (format!("{:.0}°", high)) }
                                                " / "
                                                span class="weather-value temp-low" { (format!("{:.0}°", low)) }
                                            }
                                        } @else {
                                            p class="has-text-grey" { "—" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Upcoming forecast section
            h4 class="title is-6 mb-3" {
                "Upcoming Forecast"
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
                                @if let Some(precip) = forecast.precip_chance {
                                    @if precip > 0 {
                                        p class="is-size-7 has-text-info" {
                                            (format!("{}% ", precip))
                                            span { "🌧️" }
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
}
