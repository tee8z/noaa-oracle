use maud::{html, Markup};

use super::weather_table::WeatherDisplay;

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

/// Get region display name
pub fn region_name(region: u8) -> &'static str {
    match region {
        0 => "Alaska & Hawaii",
        1 => "Pacific",
        2 => "Mountain",
        3 => "Central",
        _ => "Eastern",
    }
}

/// Mercator projection for latitude
/// Formula: ln(tan(π/4 + lat*π/360))
fn mercator_lat(lat: f64) -> f64 {
    (std::f64::consts::PI / 4.0 + lat * std::f64::consts::PI / 360.0)
        .tan()
        .ln()
}

/// Convert lat/lon to SVG coordinates for the USA map
/// The SVG is 599.96x327.28 pixels (from achord/svg-map-usa)
/// Uses Mercator projection with bounding box calibration
///
/// Bounding box (continental US):
/// - North: 49.3931°
/// - South: 24.545874°
/// - East: -66.95°
/// - West: -124.75°
fn lat_lon_to_svg(lat: f64, lon: f64) -> Option<(f64, f64)> {
    // SVG dimensions
    const SVG_WIDTH: f64 = 599.96;
    const SVG_HEIGHT: f64 = 327.28;

    // Bounding box for continental US
    const NORTH: f64 = 49.3931;
    const SOUTH: f64 = 24.545874;
    const EAST: f64 = -66.95;
    const WEST: f64 = -124.75;

    // Continental US bounds check
    if !(SOUTH..=NORTH).contains(&lat) || !(WEST..=EAST).contains(&lon) {
        return None;
    }

    // Apply Mercator projection to latitudes
    let mercator_top = mercator_lat(NORTH);
    let mercator_bottom = mercator_lat(SOUTH);
    let mercator_input = mercator_lat(lat);

    // Normalize coordinates
    let lat_normalized = (mercator_top - mercator_input) / (mercator_top - mercator_bottom);
    let lon_normalized = (lon - WEST) / (EAST - WEST);

    // Convert to SVG pixel coordinates
    let x = lon_normalized * SVG_WIDTH;
    let y = lat_normalized * SVG_HEIGHT;

    Some((x.clamp(0.0, SVG_WIDTH), y.clamp(0.0, SVG_HEIGHT)))
}

/// Weather map fragment - displays stations on a US map
pub fn weather_map(weather_data: &[WeatherDisplay]) -> Markup {
    html! {
        div class="weather-map-container" {
            // SVG map loaded as object so we can overlay markers
            div class="map-wrapper" {
                img src="/static/usa-map.svg" alt="USA Map" class="usa-map";

                // Station markers overlay - use "none" to stretch exactly like the img
                svg class="station-markers" viewBox="0 0 599.96 327.28" preserveAspectRatio="none" {
                    @for weather in weather_data {
                        @if let Some((x, y)) = lat_lon_to_svg(weather.latitude, weather.longitude) {
                            @let region = get_region(weather.longitude);
                            @let class = region_class(region);
                            circle
                                class={"station-marker " (class)}
                                cx=(format!("{:.1}", x))
                                cy=(format!("{:.1}", y))
                                r="3"
                                data-station-id=(weather.station_id)
                                data-station-name=(weather.station_name)
                                data-state=(weather.state)
                                data-iata=(weather.iata_id)
                                data-elevation=(weather.elevation_m.map(|e| format!("{:.0}", e)).unwrap_or_default())
                                data-temp-high=(weather.temp_high.map(|t| format!("{:.0}", t)).unwrap_or_default())
                                data-temp-low=(weather.temp_low.map(|t| format!("{:.0}", t)).unwrap_or_default())
                                data-wind=(weather.wind_speed.map(|w| w.to_string()).unwrap_or_default())
                                data-observed-start=(weather.observed_start)
                                data-observed-end=(weather.observed_end)
                                onclick="showStationPopup(this)" {}
                        }
                    }
                }

                // Popup container (hidden by default)
                div id="station-popup" class="station-popup" style="display: none;" {
                    div class="popup-header" {
                        strong class="popup-station-id" {}
                        span class="popup-iata tag is-iata is-small" {}
                        button class="delete is-small popup-close" onclick="hideStationPopup()" {}
                    }
                    div class="popup-name" {}

                    // 3-day compact forecast grid
                    div class="popup-forecast-grid" {
                        // Header row
                        div class="forecast-header-row" {
                            div class="forecast-col-label" {}
                            div class="forecast-col" { "Yesterday" }
                            div class="forecast-col" { "Today" }
                            div class="forecast-col" { "Tomorrow" }
                        }
                        // Temp row
                        div class="forecast-data-row" {
                            div class="forecast-col-label" {
                                span class="icon is-small" { i class="fas fa-temperature-high" {} }
                                " Temp"
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="yesterday-temp-obs" { "-" }
                                div class="fcst-value" data-field="yesterday-temp-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="today-temp-obs" { "-" }
                                div class="fcst-value" data-field="today-temp-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="tomorrow-temp-obs" { "-" }
                                div class="fcst-value" data-field="tomorrow-temp-fcst" { }
                            }
                        }
                        // Wind row
                        div class="forecast-data-row" {
                            div class="forecast-col-label" {
                                span class="icon is-small" { i class="fas fa-wind" {} }
                                " Wind"
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="yesterday-wind-obs" { "-" }
                                div class="fcst-value" data-field="yesterday-wind-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="today-wind-obs" { "-" }
                                div class="fcst-value" data-field="today-wind-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="tomorrow-wind-obs" { "-" }
                                div class="fcst-value" data-field="tomorrow-wind-fcst" { }
                            }
                        }
                        // Precipitation chance row (forecast-only)
                        div class="forecast-data-row" {
                            div class="forecast-col-label" {
                                span class="icon is-small" { i class="fas fa-percent" {} }
                                " Chance"
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="yesterday-chance-obs" { "-" }
                                div class="fcst-value" data-field="yesterday-chance-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="today-chance-obs" { "-" }
                                div class="fcst-value" data-field="today-chance-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="tomorrow-chance-obs" { "-" }
                                div class="fcst-value" data-field="tomorrow-chance-fcst" { }
                            }
                        }
                        // Precipitation row (rain)
                        div class="forecast-data-row" {
                            div class="forecast-col-label" {
                                span class="icon is-small" { i class="fas fa-cloud-rain" {} }
                                " Precip"
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="yesterday-rain-obs" { "-" }
                                div class="fcst-value" data-field="yesterday-rain-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="today-rain-obs" { "-" }
                                div class="fcst-value" data-field="today-rain-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="tomorrow-rain-obs" { "-" }
                                div class="fcst-value" data-field="tomorrow-rain-fcst" { }
                            }
                        }
                        // Snow row
                        div class="forecast-data-row" {
                            div class="forecast-col-label" {
                                span class="icon is-small" { i class="fas fa-snowflake" {} }
                                " Snow"
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="yesterday-snow-obs" { "-" }
                                div class="fcst-value" data-field="yesterday-snow-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="today-snow-obs" { "-" }
                                div class="fcst-value" data-field="today-snow-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="tomorrow-snow-obs" { "-" }
                                div class="fcst-value" data-field="tomorrow-snow-fcst" { }
                            }
                        }
                        // Humidity row
                        div class="forecast-data-row" {
                            div class="forecast-col-label" {
                                span class="icon is-small" { i class="fas fa-droplet" {} }
                                " Humidity"
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="yesterday-humidity-obs" { "-" }
                                div class="fcst-value" data-field="yesterday-humidity-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="today-humidity-obs" { "-" }
                                div class="fcst-value" data-field="today-humidity-fcst" { }
                            }
                            div class="forecast-col" {
                                div class="obs-value" data-field="tomorrow-humidity-obs" { "-" }
                                div class="fcst-value" data-field="tomorrow-humidity-fcst" { }
                            }
                        }
                    }

                    // Loading indicator
                    div class="popup-loading" style="display: none;" {
                        span class="icon is-small" { i class="fas fa-spinner fa-spin" {} }
                        " Loading..."
                    }
                }
            }
        }
    }
}
