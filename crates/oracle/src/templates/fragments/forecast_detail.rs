use maud::{Markup, html};

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
                "Forecasts and observations for " (station_id)
            }

            // Past performance section — table showing forecast vs actual
            @if !comparisons.is_empty() {
                div class="past-performance mb-5" {
                    h4 class="title is-6 mb-3" {
                        span class="icon-text" {
                            span { "Past forecast comparison" }
                            span class="tag is-light is-small ml-2" { (format!("{} days", comparisons.len().min(7))) }
                        }
                    }
                    p class="is-size-7 has-text-grey mb-2" {
                        "Latest available forecasts and observations by day in your time zone. Differences = forecast − observed; + means the forecast was higher. — means unavailable."
                    }
                    div class="table-container" {
                        table class="table is-fullwidth is-narrow is-size-7" {
                            thead {
                                tr {
                                    th { "Date" }
                                    th class="has-text-centered" colspan="2" { "Temp High" }
                                    th class="has-text-centered" colspan="2" { "Temp Low" }
                                    th class="has-text-centered" colspan="2" { "Max wind" }
                                    th class="has-text-centered" colspan="2" { "Humidity" }
                                    th class="has-text-centered" colspan="2" { "Rain" }
                                    th class="has-text-centered" colspan="2" { "Snow" }
                                }
                                tr class="past-subheader" {
                                    th {}
                                    th class="has-text-centered" { "Forecast" }
                                    th class="has-text-centered" { "Observed" }
                                    th class="has-text-centered" { "Forecast" }
                                    th class="has-text-centered" { "Observed" }
                                    th class="has-text-centered" { "Forecast" }
                                    th class="has-text-centered" { "Observed" }
                                    th class="has-text-centered" title="Forecast daily humidity range" { "Forecast range" }
                                    th class="has-text-centered" title="Relative humidity estimated from average temperature and dew point" { "Observed" }
                                    th class="has-text-centered" { "Forecast" }
                                    th class="has-text-centered" { "Observed" }
                                    th class="has-text-centered" { "Forecast" }
                                    th class="has-text-centered" title="Snow estimated from liquid precipitation using a 10:1 snow-to-liquid ratio" { "Observed est." }
                                }
                            }
                            tbody {
                                @for comp in comparisons.iter().take(7) {
                                    tr {
                                        td class="has-text-weight-semibold calendar-date" data-date=(calendar_date(&comp.date)) {
                                            (calendar_date(&comp.date))
                                        }
                                        // Temp High: forecast vs actual
                                        td class="has-text-centered" {
                                            span class="weather-value temp-high" { (format!("{}°F", comp.forecast_high)) }
                                        }
                                        td class="has-text-centered" {
                                            @if let Some(actual) = comp.actual_high {
                                                @let actual = rounded_temperature(actual);
                                                span class="weather-value temp-high" { (format!("{:.0}°F", actual)) }
                                                (diff_badge(comp.forecast_high as f64 - actual, "°F"))
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        // Temp Low: forecast vs actual
                                        td class="has-text-centered" {
                                            span class="weather-value temp-low" { (format!("{}°F", comp.forecast_low)) }
                                        }
                                        td class="has-text-centered" {
                                            @if let Some(actual) = comp.actual_low {
                                                @let actual = rounded_temperature(actual);
                                                span class="weather-value temp-low" { (format!("{:.0}°F", actual)) }
                                                (diff_badge(comp.forecast_low as f64 - actual, "°F"))
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        // Wind: forecast vs actual
                                        td class="has-text-centered" {
                                            @if let Some(w) = comp.forecast_wind {
                                                (format!("{} kt", w))
                                            } @else {
                                                span class="has-text-grey" { "—" }
                                            }
                                        }
                                        td class="has-text-centered" {
                                            @if let Some(w) = comp.actual_wind {
                                                (format!("{} kt", w))
                                                @if let Some(fw) = comp.forecast_wind {
                                                    (diff_badge(fw as f64 - w as f64, "kt"))
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
                                                    span class="has-text-grey" { (format!("{:.2}\"", r)) }
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
                                                    span class="has-text-grey" { (format!("{:.2}\"", r)) }
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
                                                    span class="has-text-grey" { (format!("{:.1}\"", s)) }
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
                                                    span class="has-text-grey" { (format!("{:.1}\"", s)) }
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
                    "Upcoming forecast"
                }
                p class="is-size-7 has-text-grey mb-2" { "Forecast values by date in your time zone." }
                @if forecasts.is_empty() {
                    p class="has-text-grey" { "No forecast data available." }
                } @else {
                    div class="columns is-multiline is-mobile" {
                        @for forecast in forecasts.iter().take(7) {
                            div class="column is-one-fifth-desktop is-half-mobile" {
                                div class="box forecast-day has-text-centered p-2" {
                                    p class="is-size-7 has-text-weight-semibold mb-1 calendar-date" data-date=(calendar_date(&forecast.date)) {
                                        (calendar_date(&forecast.date))
                                    }
                                    // Temperature
                                    p class="mb-1" {
                                        span class="weather-value temp-high" { (format!("{}°F", forecast.temp_high)) }
                                        " / "
                                        span class="weather-value temp-low" { (format!("{}°F", forecast.temp_low)) }
                                    }
                                    // Wind
                                    @if let Some(wind) = forecast.wind_speed {
                                        p class="is-size-7" {
                                            (format!("{} kt", wind))
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
                                        p class="is-size-7 has-text-info" {
                                            (format!("{}% precip chance", precip))
                                        }
                                    }
                                    // Precipitation amount
                                    @if let Some(precip_amt) = forecast.rain_amt {
                                        p class="is-size-7 has-text-info" {
                                            (format!("{:.2}\" rain", precip_amt))
                                        }
                                    }
                                    // Snow amount
                                    @if let Some(snow) = forecast.snow_amt {
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

/// Preserve the UTC calendar date when the data includes a midnight timestamp.
fn calendar_date(date: &str) -> &str {
    date.split([' ', 'T']).next().unwrap_or(date)
}

/// Render the signed forecast error with its unit and sign convention.
fn rounded_temperature(value: f64) -> f64 {
    let rounded = value.round();
    if rounded == 0.0 { 0.0 } else { rounded }
}

fn diff_badge(diff: f64, unit: &str) -> Markup {
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
        span class=(format!("is-size-7 {}", class))
            title=(format!("Forecast − observed: {:+.1} {}. Positive means the forecast was higher.", diff, unit)) {
            (format!("{:+.0} {}", diff, unit))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn past_comparisons_round_temperature_halves_like_scoring() {
        let comparison = ForecastComparison {
            date: "2026-09-19".into(),
            forecast_high: 55,
            forecast_low: -3,
            forecast_wind: None,
            forecast_humidity_max: None,
            forecast_humidity_min: None,
            forecast_rain: None,
            forecast_snow: None,
            actual_high: Some(54.5),
            actual_low: Some(-2.5),
            actual_wind: None,
            actual_humidity: None,
            actual_rain: None,
            actual_snow: None,
        };
        let html = forecast_detail("KPWM", &[comparison], &[]).into_string();
        assert_eq!(html.matches("55°F").count(), 2);
        assert_eq!(html.matches("-3°F").count(), 2);
        assert!(!html.contains("54°F"));
        assert!(!html.contains("-2°F"));
        assert!(!html.contains("Forecast − observed:"));
    }
}
