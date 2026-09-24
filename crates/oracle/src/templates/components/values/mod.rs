//! Weather values, formatted the same way on every page.

use maud::{Markup, html};

/// Whole degrees, rounding halves away from zero like scoring, without "-0".
pub fn whole_degrees(value: f64) -> f64 {
    let rounded = value.round();
    if rounded == 0.0 { 0.0 } else { rounded }
}

pub fn missing() -> Markup {
    html! { span class="val is-missing" title="Unavailable" { "—" } }
}

/// A temperature; `class` is `temp-high`, `temp-low` or empty.
pub fn temperature(value: Option<f64>, class: &str) -> Markup {
    match value {
        Some(value) => html! {
            span class={ "val " (class) } { (format!("{:.0}°F", whole_degrees(value))) }
        },
        None => missing(),
    }
}

/// How the difference reads: settled, or provisional while the observation
/// window is still open and the observed high and low can still move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Settled {
    Final,
    SoFar,
}

/// Observed − forecast, beside the observed value: positive means it came
/// in higher than forecast.
pub fn difference(observed: Option<f64>, forecast: Option<f64>, unit: &str, settled: Settled) -> Markup {
    let (Some(observed), Some(forecast)) = (observed, forecast) else {
        return missing();
    };
    let difference = if unit == "°F" {
        whole_degrees(observed) - forecast
    } else {
        observed - forecast
    };
    let class = match settled {
        Settled::SoFar => "delta is-provisional",
        Settled::Final => accuracy_class(difference),
    };
    let title = match settled {
        Settled::Final => format!("Observed − forecast: {difference:+.0} {unit}"),
        Settled::SoFar => format!(
            "Observed so far − forecast: {difference:+.0} {unit}. The day is not over, so this can still change."
        ),
    };
    html! {
        span class=(class) title=(title) { (format!("{difference:+.0}{unit}")) }
    }
}

/// Within 3 is good, within 6 is fair, beyond that is poor.
pub fn accuracy_class(difference: f64) -> &'static str {
    match difference.abs() {
        diff if diff <= 3.0 => "delta is-close",
        diff if diff <= 6.0 => "delta is-off",
        _ => "delta is-far",
    }
}

/// Rain or snow in inches. Zero is dimmed so real amounts stand out.
pub fn precipitation(value: Option<f64>, kind: &str, decimals: usize) -> Markup {
    match value {
        Some(amount) if amount <= 0.0 => html! {
            span class="val is-zero" { (format!("{:.*}\"", decimals, 0.0)) }
        },
        Some(amount) => html! {
            span class={ "val " (kind) } { (format!("{amount:.decimals$}\"")) }
        },
        None => missing(),
    }
}

pub fn wind(speed: Option<i64>, direction: Option<i64>) -> Markup {
    match speed {
        Some(speed) => html! {
            span class="val" {
                (speed) " kt"
                @if let Some(label) = direction.and_then(compass) {
                    " " span class="compass" { (label) }
                }
            }
        },
        None => missing(),
    }
}

pub fn percent(value: Option<i64>) -> Markup {
    match value {
        Some(value) => html! { span class="val" { (value) "%" } },
        None => missing(),
    }
}

pub fn compass(degrees: i64) -> Option<&'static str> {
    Some(match degrees {
        0..=22 | 338..=360 => "N",
        23..=67 => "NE",
        68..=112 => "E",
        113..=157 => "SE",
        158..=202 => "S",
        203..=247 => "SW",
        248..=292 => "W",
        293..=337 => "NW",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differences_are_observed_minus_forecast() {
        let warmer = difference(Some(75.0), Some(70.0), "°F", Settled::Final).into_string();
        assert!(warmer.contains("+5°F") && warmer.contains("is-off"), "{warmer}");
        let cooler = difference(Some(75.0), Some(83.0), "°F", Settled::Final).into_string();
        assert!(cooler.contains("-8°F") && cooler.contains("is-far"), "{cooler}");
        // Halves round like scoring before comparing.
        let half = difference(Some(54.5), Some(55.0), "°F", Settled::Final).into_string();
        assert!(half.contains("+0°F"), "{half}");
    }

    #[test]
    fn open_windows_show_a_quiet_difference() {
        let html = difference(Some(60.0), Some(80.0), "°F", Settled::SoFar).into_string();
        assert!(html.contains("-20°F"));
        assert!(html.contains("is-provisional"));
        assert!(!html.contains("is-far"));
    }

    #[test]
    fn zero_precipitation_is_dimmed() {
        assert!(precipitation(Some(0.0), "rain", 2).into_string().contains("is-zero"));
        let rain = precipitation(Some(0.25), "rain", 2).into_string();
        assert!(rain.contains("0.25\"") && !rain.contains("is-zero"));
        assert!(precipitation(None, "rain", 2).into_string().contains("—"));
    }
}
