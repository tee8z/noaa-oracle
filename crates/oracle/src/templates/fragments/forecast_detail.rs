use maud::{html, Markup};

/// Forecast data for display
pub struct ForecastDisplay {
    pub date: String,
    pub temp_high: i64,
    pub temp_low: i64,
    pub wind_speed: Option<i64>,
    /// Wind direction in degrees (0-360, where 0/360 = North)
    pub wind_direction: Option<i64>,
    /// Maximum relative humidity (percent)
    pub humidity_max: Option<i64>,
    /// Minimum relative humidity (percent)
    pub humidity_min: Option<i64>,
    pub precip_chance: Option<i64>,
    /// Rain amount in inches
    pub rain_amt: Option<f64>,
    /// Snow amount in inches
    pub snow_amt: Option<f64>,
}

/// Comparison of forecast vs actual observation for a past day
pub struct ForecastComparison {
    pub date: String,
    // Forecast values
    pub forecast_high: i64,
    pub forecast_low: i64,
    pub forecast_wind: Option<i64>,
    pub forecast_humidity_max: Option<i64>,
    pub forecast_humidity_min: Option<i64>,
    pub forecast_precip_chance: Option<i64>,
    pub forecast_rain: Option<f64>,
    pub forecast_snow: Option<f64>,
    // Actual observed values
    pub actual_high: Option<f64>,
    pub actual_low: Option<f64>,
    pub actual_wind: Option<i64>,
    pub actual_humidity: Option<i64>,
    pub actual_rain: Option<f64>,
    pub actual_snow: Option<f64>,
}

/// Forecast detail fragment - shown when a weather row is expanded
pub fn forecast_detail(
    station_id: &str,
    comparisons: &[ForecastComparison],
    forecasts: &[ForecastDisplay],
) -> Markup {
    html! {
        div class="forecast-detail p-3" {
            h3 class="title is-5 mb-4" {
                "Forecast for " (station_id)
            }

            // Past performance section — table showing forecast vs actual
            @if !comparisons.is_empty() {
                div class="past-performance mb-5" {
                    h4 class="title is-6 mb-3" {
                        span class="icon-text" {
                            span { "Past Performance" }
                            span class="tag is-light is-small ml-2" { (format!("{} days", comparisons.len())) }
                        }
                    }
                    div class="table-container" {
                        table class="table is-fullwidth is-narrow is-size-7" {
                            thead {
                                tr {
                                    th { "Date" }
                                    th class="has-text-centered" colspan="2" { "Temp High" }
                                    th class="has-text-centered" colspan="2" { "Temp Low" }
                                    th class="has-text-centered" colspan="2" { "Wind" }
                                    th class="has-text-centered" colspan="2" { "Humidity" }
                                    th class="has-text-centered" colspan="2" { "Rain" }
                                    th class="has-text-centered" colspan="2" { "Snow" }
                                }
                                tr class="past-subheader" {
                                    th {}
                                    th class="has-text-centered" { "Fcst" }
                                    th class="has-text-centered" { "Actual" }
                                    th class="has-text-centered" { "Fcst" }
                                    th class="has-text-centered" { "Actual" }
                                    th class="has-text-centered" { "Fcst" }
                                    th class="has-text-centered" { "Actual" }
                                    th class="has-text-centered" { "Fcst" }
                                    th class="has-text-centered" { "Actual" }
                                    th class="has-text-centered" { "Fcst" }
                                    th class="has-text-centered" { "Actual" }
                                    th class="has-text-centered" { "Fcst" }
                                    th class="has-text-centered" { "Actual" }
                                }
                            }
                            tbody {
                                @for comp in comparisons.iter().take(7) {
                                    tr {
                                        td class="has-text-weight-semibold local-date" data-utc=(comp.date.clone()) {
                                            (comp.date.clone())
                                        }
                                        // Temp High: forecast vs actual
                                        td class="has-text-centered" {
                                            span class="weather-value temp-high" { (format!("{}°", comp.forecast_high)) }
                                        }
                                        td class="has-text-centered" {
                                            @if let Some(actual) = comp.actual_high {
                                                span class="weather-value temp-high" { (format!("{:.0}°", actual)) }
                                                (diff_badge(comp.forecast_high as f64 - actual))
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        // Temp Low: forecast vs actual
                                        td class="has-text-centered" {
                                            span class="weather-value temp-low" { (format!("{}°", comp.forecast_low)) }
                                        }
                                        td class="has-text-centered" {
                                            @if let Some(actual) = comp.actual_low {
                                                span class="weather-value temp-low" { (format!("{:.0}°", actual)) }
                                                (diff_badge(comp.forecast_low as f64 - actual))
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        // Wind: forecast vs actual
                                        td class="has-text-centered" {
                                            @if let Some(w) = comp.forecast_wind {
                                                (format!("{}", w))
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        td class="has-text-centered" {
                                            @if let Some(w) = comp.actual_wind {
                                                (format!("{}", w))
                                                @if let Some(fw) = comp.forecast_wind {
                                                    (diff_badge(fw as f64 - w as f64))
                                                }
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        // Humidity: forecast vs actual
                                        td class="has-text-centered" {
                                            @if let (Some(hmin), Some(hmax)) = (comp.forecast_humidity_min, comp.forecast_humidity_max) {
                                                (format!("{}-{}%", hmin, hmax))
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        td class="has-text-centered" {
                                            @if let Some(h) = comp.actual_humidity {
                                                (format!("{}%", h))
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        // Rain: forecast vs actual
                                        td class="has-text-centered" {
                                            @if let Some(r) = comp.forecast_rain {
                                                @if r > 0.0 {
                                                    span class="has-text-info" { (format!("{:.2}\"", r)) }
                                                } @else {
                                                    span class="has-text-grey" { "—" }
                                                }
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        td class="has-text-centered" {
                                            @if let Some(r) = comp.actual_rain {
                                                @if r > 0.0 {
                                                    span class="has-text-info" { (format!("{:.2}\"", r)) }
                                                } @else {
                                                    span class="has-text-grey" { "—" }
                                                }
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        // Snow: forecast vs actual
                                        td class="has-text-centered" {
                                            @if let Some(s) = comp.forecast_snow {
                                                @if s > 0.0 {
                                                    span class="has-text-link" { (format!("{:.1}\"", s)) }
                                                } @else {
                                                    span class="has-text-grey" { "—" }
                                                }
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        td class="has-text-centered" {
                                            @if let Some(s) = comp.actual_snow {
                                                @if s > 0.0 {
                                                    span class="has-text-link" { (format!("{:.1}\"", s)) }
                                                } @else {
                                                    span class="has-text-grey" { "—" }
                                                }
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Upcoming forecast section
            div class="upcoming-forecast" {
                h4 class="title is-6 mb-3" {
                    "Upcoming Forecast"
                }
                @if forecasts.is_empty() {
                    p class="has-text-grey" { "No forecast data available." }
                } @else {
                    div class="columns is-multiline is-mobile" {
                        @for forecast in forecasts.iter().take(7) {
                            div class="column is-one-fifth-desktop is-half-mobile" {
                                div class="box forecast-day has-text-centered p-2" {
                                    p class="is-size-7 has-text-weight-semibold mb-1 local-date" data-utc=(forecast.date.clone()) {
                                        (forecast.date.clone())
                                    }
                                    // Temperature
                                    p class="mb-1" {
                                        span class="weather-value temp-high" { (format!("{}°", forecast.temp_high)) }
                                        " / "
                                        span class="weather-value temp-low" { (format!("{}°", forecast.temp_low)) }
                                    }
                                    // Wind
                                    @if let Some(wind) = forecast.wind_speed {
                                        p class="is-size-7" {
                                            (format!("{} mph", wind))
                                            @if let Some(dir) = forecast.wind_direction {
                                                " "
                                                span class="has-text-grey" { (wind_direction_label(dir)) }
                                            }
                                        }
                                    }
                                    // Humidity
                                    @if let (Some(hmax), Some(hmin)) = (forecast.humidity_max, forecast.humidity_min) {
                                        p class="is-size-7 has-text-grey" {
                                            (format!("{}%-{}% RH", hmin, hmax))
                                        }
                                    }
                                    // Precipitation chance
                                    @if let Some(precip) = forecast.precip_chance {
                                        @if precip > 0 {
                                            p class="is-size-7 has-text-info" {
                                                (format!("{}% chance", precip))
                                            }
                                        }
                                    }
                                    // Precipitation amount
                                    @if let Some(precip_amt) = forecast.rain_amt {
                                        @if precip_amt > 0.0 {
                                            p class="is-size-7 has-text-info" {
                                                (format!("{:.2}\" precip", precip_amt))
                                            }
                                        }
                                    }
                                    // Snow amount
                                    @if let Some(snow) = forecast.snow_amt {
                                        @if snow > 0.0 {
                                            p class="is-size-7 has-text-link" {
                                                (format!("{:.1}\" snow", snow))
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
}

/// Render a small colored diff badge (e.g. "+3°" in green, "-5°" in red)
fn diff_badge(diff: f64) -> Markup {
    if diff.abs() <= 0.5 {
        return html! {};
    }
    let class = if diff.abs() <= 3.0 {
        "has-text-success"
    } else if diff.abs() <= 6.0 {
        "has-text-warning"
    } else {
        "has-text-danger"
    };
    html! {
        " "
        span class=(format!("is-size-7 {}", class)) {
            (format!("{:+.0}", diff))
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
