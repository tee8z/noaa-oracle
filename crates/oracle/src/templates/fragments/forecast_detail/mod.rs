use maud::{Markup, PreEscaped, html};

use crate::templates::components::{
    time as when,
    values::{self, Settled},
};

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

/// A station's recent forecasts against what was observed, and its coming
/// week. Shown when a list row opens or a map pin is clicked.
pub fn forecast_detail(
    station_id: &str,
    comparisons: &[ForecastComparison],
    forecasts: &[ForecastDisplay],
) -> Markup {
    html! {
        div class="forecast-detail" {
            h3 class="is-sr-only" { "Forecasts and observations for " (station_id) }
            @if !comparisons.is_empty() {
                section class="past-performance" {
                    h4 class="title is-6" { "Past week: forecast vs observed" }
                    p class="forecast-note" {
                        "Latest available forecasts by your calendar day (UTC if unavailable). Differences are observed − forecast; + means it came in higher."
                    }
                    div class="table-container" {
                        table class="table is-narrow is-fullwidth past-table" {
                            thead {
                                tr {
                                    th { "Day" }
                                    th { "High" }
                                    th { "Low" }
                                    th { "Max wind" }
                                    th title="Forecast range; observed is estimated from temperature and dew point" { "Humidity" }
                                    th { "Rain" }
                                    th title="Observed snow is estimated from liquid precipitation at 10:1" { "Snow" }
                                }
                            }
                            tbody {
                                @for comp in comparisons.iter().take(7) {
                                    tr {
                                        th scope="row" { (when::calendar_day(&comp.date)) }
                                        td {
                                            (pair(values::temperature(comp.actual_high, "temp-high"),
                                                  values::temperature(Some(comp.forecast_high as f64), "temp-high"),
                                                  comp.actual_high.map(|_| values::difference(comp.actual_high, Some(comp.forecast_high as f64), "°F", Settled::Final))))
                                        }
                                        td {
                                            (pair(values::temperature(comp.actual_low, "temp-low"),
                                                  values::temperature(Some(comp.forecast_low as f64), "temp-low"),
                                                  comp.actual_low.map(|_| values::difference(comp.actual_low, Some(comp.forecast_low as f64), "°F", Settled::Final))))
                                        }
                                        td {
                                            (pair(values::wind(comp.actual_wind, None),
                                                  values::wind(comp.forecast_wind, None),
                                                  comp.actual_wind.zip(comp.forecast_wind).map(|(observed, forecast)| values::difference(Some(observed as f64), Some(forecast as f64), " kt", Settled::Final))))
                                        }
                                        td {
                                            (pair(values::percent(comp.actual_humidity),
                                                  humidity_range(comp.forecast_humidity_min, comp.forecast_humidity_max),
                                                  None))
                                        }
                                        td {
                                            (pair(values::precipitation(comp.actual_rain, "rain", 2),
                                                  values::precipitation(comp.forecast_rain, "rain", 2),
                                                  None))
                                        }
                                        td {
                                            (pair(values::precipitation(comp.actual_snow, "snow", 1),
                                                  values::precipitation(comp.forecast_snow, "snow", 1),
                                                  None))
                                        }
                                    }
                                }
                            }
                        }
                    }
                    p class="forecast-note" { "Each cell: observed, then " span class="fcst" { "forecast" } "." }
                }
            }

            section class="upcoming-forecast" {
                h4 class="title is-6" { "Coming days" }
                @if forecasts.is_empty() {
                    p class="muted" { "No forecast data available." }
                } @else {
                    ol class="forecast-days" {
                        @for forecast in forecasts.iter().take(7) {
                            li class="forecast-day" {
                                p class="forecast-date" { (when::calendar_day(&forecast.date)) }
                                p {
                                    (values::temperature(Some(forecast.temp_high as f64), "temp-high"))
                                    " / "
                                    (values::temperature(Some(forecast.temp_low as f64), "temp-low"))
                                }
                                @if forecast.wind_speed.is_some() {
                                    p { (values::wind(forecast.wind_speed, forecast.wind_direction)) }
                                }
                                @if let (Some(min), Some(max)) = (forecast.humidity_min, forecast.humidity_max) {
                                    p class="muted" { (min) "–" (max) "% RH" }
                                }
                                @if let Some(chance) = forecast.precip_chance {
                                    p { span class=(if chance > 0 { "val rain" } else { "val is-zero" }) { (chance) "% chance" } }
                                }
                                @if forecast.rain_amt.is_some() {
                                    p { (values::precipitation(forecast.rain_amt, "rain", 2)) " rain" }
                                }
                                @if forecast.snow_amt.is_some() {
                                    p { (values::precipitation(forecast.snow_amt, "snow", 1)) " snow" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A map pin's station: its name, then the (cached) forecast detail.
pub fn station_detail(station_id: &str, place: Option<&str>, forecast_html: &str) -> Markup {
    html! {
        div class="station-detail" {
            div class="station-detail-head" {
                h3 class="title is-6 mb-0" { (station_id) }
                @if let Some(place) = place {
                    span class="muted" { (place) }
                }
                a href=(format!("/?view=list&q={station_id}")) class="is-size-7" { "Show in the list" }
            }
            (PreEscaped(forecast_html))
        }
    }
}

/// Observed value, its difference, and the forecast underneath in grey.
fn pair(observed: Markup, forecast: Markup, difference: Option<Markup>) -> Markup {
    html! {
        span class="obs" { (observed) @if let Some(difference) = difference { " " (difference) } }
        span class="fcst" { (forecast) }
    }
}

fn humidity_range(min: Option<i64>, max: Option<i64>) -> Markup {
    match (min, max) {
        (Some(min), Some(max)) => html! { span class="val" { (min) "–" (max) "%" } },
        _ => values::missing(),
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
        // Equal after rounding: the difference reads +0.
        assert!(html.contains("+0°F"));
    }

    #[test]
    fn past_differences_are_observed_minus_forecast() {
        let comparison = ForecastComparison {
            date: "2026-09-19".into(),
            forecast_high: 75,
            forecast_low: 60,
            forecast_wind: Some(10),
            forecast_humidity_max: None,
            forecast_humidity_min: None,
            forecast_rain: None,
            forecast_snow: None,
            actual_high: Some(80.0),
            actual_low: Some(58.0),
            actual_wind: Some(14),
            actual_humidity: None,
            actual_rain: None,
            actual_snow: None,
        };
        let html = forecast_detail("KPWM", &[comparison], &[]).into_string();
        assert!(html.contains("+5°F"), "{html}");
        assert!(html.contains("-2°F"), "{html}");
        assert!(html.contains("+4 kt"), "{html}");
    }
}
