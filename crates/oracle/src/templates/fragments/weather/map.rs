//! Stations on a map of the lower 48, coloured by their latest temperature.
//! Hovering a pin shows its values; clicking loads the station's forecast
//! and history below the map.

use maud::{Markup, html};

use super::{WeatherContext, WeatherDisplay, list::no_data, place};
use crate::templates::{
    assets,
    components::{time as when, values::whole_degrees},
};

/// Diverging bands, cold blue through a neutral middle to hot red.
const BANDS: [(&str, &str); 5] = [
    ("t-cold", "Below 40°F"),
    ("t-cool", "40–59°F"),
    ("t-mild", "60–74°F"),
    ("t-warm", "75–89°F"),
    ("t-hot", "90°F and above"),
];

fn band(temperature: Option<f64>) -> &'static str {
    match temperature.map(whole_degrees) {
        None => "t-none",
        Some(t) if t < 40.0 => "t-cold",
        Some(t) if t < 60.0 => "t-cool",
        Some(t) if t < 75.0 => "t-mild",
        Some(t) if t < 90.0 => "t-warm",
        Some(_) => "t-hot",
    }
}

/// Mercator projection for latitude: ln(tan(π/4 + lat·π/360))
fn mercator_lat(lat: f64) -> f64 {
    (std::f64::consts::PI / 4.0 + lat * std::f64::consts::PI / 360.0)
        .tan()
        .ln()
}

/// Latitude and longitude to coordinates on the 599.96×327.28 USA map
/// (achord/svg-map-usa), Mercator between the continental US bounds.
/// Stations outside the lower 48 are not on the map.
fn lat_lon_to_svg(lat: f64, lon: f64) -> Option<(f64, f64)> {
    const SVG_WIDTH: f64 = 599.96;
    const SVG_HEIGHT: f64 = 327.28;
    const NORTH: f64 = 49.3931;
    const SOUTH: f64 = 24.545874;
    const EAST: f64 = -66.95;
    const WEST: f64 = -124.75;

    if !(SOUTH..=NORTH).contains(&lat) || !(WEST..=EAST).contains(&lon) {
        return None;
    }
    let top = mercator_lat(NORTH);
    let bottom = mercator_lat(SOUTH);
    let y = (top - mercator_lat(lat)) / (top - bottom) * SVG_HEIGHT;
    let x = (lon - WEST) / (EAST - WEST) * SVG_WIDTH;
    Some((x.clamp(0.0, SVG_WIDTH), y.clamp(0.0, SVG_HEIGHT)))
}

/// What a pin says on hover.
fn summary(weather: &WeatherDisplay, context: &WeatherContext) -> String {
    let degrees = |value: Option<f64>| {
        value.map_or("—".to_string(), |t| format!("{:.0}°F", whole_degrees(t)))
    };
    let mut text = format!("{} {}", weather.station_id, place(weather));
    text.push_str(&format!("\nLatest {}", degrees(weather.latest_temp)));
    if let Some(time) = weather.latest_temp_time.as_deref().and_then(when::parse) {
        text.push_str(&format!(", {}", when::ago(time, context.now)));
    }
    text.push_str(&format!(
        "\nHigh {} · Low {}",
        degrees(weather.temp_high),
        degrees(weather.temp_low)
    ));
    if weather.forecast_high.is_some() || weather.forecast_low.is_some() {
        text.push_str(&format!(
            "\nForecast {} / {}",
            degrees(weather.forecast_high.map(|t| t as f64)),
            degrees(weather.forecast_low.map(|t| t as f64))
        ));
    }
    text
}

fn station_url(station_id: &str) -> String {
    format!("/fragments/station/{station_id}")
}

pub(super) fn weather_map(weather: &[WeatherDisplay], context: &WeatherContext) -> Markup {
    if weather.is_empty() {
        return no_data();
    }
    let off_map: Vec<_> = weather
        .iter()
        .filter(|w| lat_lon_to_svg(w.latitude, w.longitude).is_none())
        .collect();
    html! {
        div class="wx-map" {
            div class="map-wrapper" {
                img src=(assets::USA_MAP_URL) alt="" class="usa-map";
                svg class="station-markers" viewBox="0 0 599.96 327.28" preserveAspectRatio="none"
                    role="group" aria-label="Stations by latest temperature" {
                    @for station in weather {
                        @if let Some((x, y)) = lat_lon_to_svg(station.latitude, station.longitude) {
                            g class={ "pin " (band(station.latest_temp)) }
                              tabindex="0" role="button"
                              aria-label=(summary(station, context).replace('\n', ", "))
                              hx-get=(station_url(&station.station_id))
                              hx-target="#map-station"
                              hx-trigger="click, keyup[key=='Enter']"
                              hx-indicator="#map-station-loading" {
                                title { (summary(station, context)) }
                                // A wider invisible circle is easier to hit.
                                circle class="pin-target" cx=(format!("{x:.1}")) cy=(format!("{y:.1}")) r="9" {}
                                circle class="pin-dot" cx=(format!("{x:.1}")) cy=(format!("{y:.1}")) r="4.5" {}
                            }
                        }
                    }
                }
            }
            ul class="map-legend" aria-label="Latest temperature" {
                @for (class, label) in BANDS {
                    li { span class={ "swatch " (class) } {} (label) }
                }
                li { span class="swatch t-none" {} "No report" }
            }
            @if !off_map.is_empty() {
                p class="map-off" {
                    "Not on the map: "
                    @for (index, station) in off_map.iter().enumerate() {
                        @if index > 0 { ", " }
                        a href=(format!("/?view=list&q={}", station.station_id))
                          hx-get=(station_url(&station.station_id))
                          hx-target="#map-station"
                          hx-indicator="#map-station-loading"
                          title=(summary(station, context)) {
                            (station.station_id)
                        }
                    }
                }
            }
            p id="map-station-loading" class="htmx-indicator map-loading" role="status" {
                span class="loader" {} " Loading station…"
            }
            div id="map-station" aria-live="polite" {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_are_coloured_by_whole_degrees() {
        assert_eq!(band(None), "t-none");
        assert_eq!(band(Some(39.4)), "t-cold");
        assert_eq!(band(Some(39.5)), "t-cool");
        assert_eq!(band(Some(74.0)), "t-mild");
        assert_eq!(band(Some(89.6)), "t-hot");
    }

    #[test]
    fn only_the_lower_48_are_on_the_map() {
        assert!(lat_lon_to_svg(41.97, -87.9).is_some());
        assert!(lat_lon_to_svg(61.17, -150.0).is_none());
        assert!(lat_lon_to_svg(21.3, -157.9).is_none());
    }
}
