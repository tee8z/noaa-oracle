use crate::helpers::{spawn_app, MockWeatherAccess};
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use hyper::{header, Method};
use oracle::{Forecast, Observation, Station, TemperatureUnit};
use std::sync::Arc;
use tower::ServiceExt;

/// Test that the dashboard endpoint returns HTML with weather data
#[tokio::test]
async fn dashboard_returns_current_day_observations() {
    let mut weather_data = MockWeatherAccess::new();

    // The dashboard should request observations for the last 24 hours only
    weather_data
        .expect_observation_data()
        .withf(|req, _| {
            // Verify that start and end times are set (not None)
            // This ensures we're filtering to current day, not all historical data
            req.start.is_some() && req.end.is_some()
        })
        .times(1)
        .returning(|_, _| Ok(mock_observation_data()));

    weather_data
        .expect_stations()
        .times(1)
        .returning(|| Ok(mock_stations()));

    let test_app = spawn_app(Arc::new(weather_data)).await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();

    let response = test_app
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("Failed to execute request.");

    assert!(response.status().is_success());

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Verify the response contains weather data
    assert!(html.contains("Current Weather"));
    // Should contain our mock station
    assert!(html.contains("KORD"));
}

/// Test that the weather fragment endpoint filters by time range
#[tokio::test]
async fn weather_fragment_uses_24_hour_window() {
    let mut weather_data = MockWeatherAccess::new();

    weather_data
        .expect_observation_data()
        .withf(|req, _| {
            // The fragment handler should use a 24-hour window
            if let (Some(start), Some(end)) = (req.start, req.end) {
                let duration = end - start;
                // Should be approximately 24 hours (allow some tolerance)
                duration.whole_hours() >= 23 && duration.whole_hours() <= 25
            } else {
                false
            }
        })
        .times(1)
        .returning(|_, _| Ok(mock_observation_data()));

    weather_data
        .expect_stations()
        .times(1)
        .returning(|| Ok(mock_stations()));

    let test_app = spawn_app(Arc::new(weather_data)).await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/fragments/weather")
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();

    let response = test_app
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("Failed to execute request.");

    assert!(response.status().is_success());
}

/// Test that the forecast fragment endpoint returns forecast data
#[tokio::test]
async fn forecast_fragment_returns_forecast_data() {
    let mut weather_data = MockWeatherAccess::new();

    weather_data
        .expect_forecasts_data()
        .withf(|req, station_ids| {
            // Should request forecasts starting from now
            req.start.is_some() && req.end.is_some() && station_ids.contains(&"KORD".to_string())
        })
        .times(1)
        .returning(|_, _| Ok(mock_forecast_data()));

    let test_app = spawn_app(Arc::new(weather_data)).await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/fragments/forecast/KORD")
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();

    let response = test_app
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("Failed to execute request.");

    assert!(response.status().is_success());

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Verify the response contains forecast data
    assert!(html.contains("Forecast for KORD"));
    // Should contain temperature values from our mock data
    assert!(html.contains("75"));
    assert!(html.contains("55"));
}

/// Test that forecast fragment handles missing data gracefully
#[tokio::test]
async fn forecast_fragment_handles_no_data() {
    let mut weather_data = MockWeatherAccess::new();

    weather_data
        .expect_forecasts_data()
        .times(1)
        .returning(|_, _| Ok(vec![]));

    let test_app = spawn_app(Arc::new(weather_data)).await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/fragments/forecast/KXYZ")
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();

    let response = test_app
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("Failed to execute request.");

    assert!(response.status().is_success());

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Should show a message about no data
    assert!(html.contains("No forecast data available"));
}

/// Test that dashboard handles empty weather data gracefully
#[tokio::test]
async fn dashboard_handles_no_weather_data() {
    let mut weather_data = MockWeatherAccess::new();

    weather_data
        .expect_observation_data()
        .times(1)
        .returning(|_, _| Ok(vec![]));

    weather_data
        .expect_stations()
        .times(1)
        .returning(|| Ok(vec![]));

    let test_app = spawn_app(Arc::new(weather_data)).await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();

    let response = test_app
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("Failed to execute request.");

    assert!(response.status().is_success());

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Should show a message about no data
    assert!(html.contains("No weather data available"));
}

fn mock_observation_data() -> Vec<Observation> {
    vec![Observation {
        station_id: String::from("KORD"),
        start_time: String::from("2024-08-12T00:00:00+00:00"),
        end_time: String::from("2024-08-12T23:59:59+00:00"),
        temp_low: 55.0,
        temp_high: 75.0,
        wind_speed: 10,
        temp_unit_code: TemperatureUnit::Fahrenheit.to_string(),
    }]
}

fn mock_forecast_data() -> Vec<Forecast> {
    vec![
        Forecast {
            station_id: String::from("KORD"),
            date: String::from("2024-08-13"),
            start_time: String::from("2024-08-13T00:00:00+00:00"),
            end_time: String::from("2024-08-14T00:00:00+00:00"),
            temp_low: 55,
            temp_high: 75,
            wind_speed: Some(12),
            temp_unit_code: TemperatureUnit::Fahrenheit.to_string(),
            precip_chance: None,
        },
        Forecast {
            station_id: String::from("KORD"),
            date: String::from("2024-08-14"),
            start_time: String::from("2024-08-14T00:00:00+00:00"),
            end_time: String::from("2024-08-15T00:00:00+00:00"),
            temp_low: 58,
            temp_high: 78,
            wind_speed: Some(8),
            temp_unit_code: TemperatureUnit::Fahrenheit.to_string(),
            precip_chance: None,
        },
    ]
}

fn mock_stations() -> Vec<Station> {
    vec![Station {
        station_id: String::from("KORD"),
        station_name: String::from("Chicago O'Hare"),
        state: String::from("IL"),
        iata_id: String::from("ORD"),
        elevation_m: Some(205.0),
        latitude: 41.9742,
        longitude: -87.9073,
    }]
}
