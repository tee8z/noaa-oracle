use crate::helpers::{MockWeatherAccess, spawn_app};
use axum::http::{Method, header};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use oracle::{Forecast, Observation, Station, TemperatureUnit};
use std::sync::{Arc, Mutex};
use time::{Duration, OffsetDateTime, Time, format_description::well_known::Rfc3339};
use tower::ServiceExt;

/// Test that the dashboard endpoint returns HTML with weather data
#[tokio::test]
async fn dashboard_returns_current_day_observations() {
    let mut weather_data = MockWeatherAccess::new();

    // Both page and refresh load the UTC day plus a recent latest-report window.
    expect_current_observations(&mut weather_data);

    weather_data
        .expect_stations()
        .times(1)
        .returning(|| Ok(mock_stations()));

    // Dashboard now batch-fetches forecast accuracy for displayed stations
    weather_data
        .expect_forecasts_data()
        .times(1)
        .returning(|_, _| Ok(vec![]));

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
    assert!(html.contains("Latest observed"));
    assert!(html.contains("63°F"));
    assert!(html.contains("2024-08-12T23:53:00Z"));
    assert!(html.contains("Today so far (UTC)"));
}

/// Test that the weather fragment endpoint filters by time range
#[tokio::test]
async fn weather_fragment_uses_same_current_day_and_latest_report_windows() {
    let mut weather_data = MockWeatherAccess::new();

    expect_current_observations(&mut weather_data);

    weather_data
        .expect_stations()
        .times(1)
        .returning(|| Ok(mock_stations()));

    // Weather fragment now batch-fetches forecast accuracy
    weather_data
        .expect_forecasts_data()
        .times(1)
        .returning(|_, _| Ok(vec![]));

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

#[tokio::test]
async fn selected_utc_day_uses_its_previous_day_forecast_and_preserves_refresh_context() {
    let start = OffsetDateTime::parse("2024-08-12T00:00:00Z", &Rfc3339).unwrap();
    let end = start + Duration::days(1);
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_observation_data()
        .withf(move |request, stations| {
            request.start == Some(start)
                && request.end == Some(end - Duration::nanoseconds(1))
                && request.station_ids == "KORD"
                && stations == &["KORD"]
        })
        .times(2)
        .returning(|_, _| Ok(mock_observation_data()));
    weather
        .expect_forecasts_data()
        .withf(move |request, stations| {
            request.start == Some(start)
                && request.end == Some(end)
                && request.generated_start == Some(start - Duration::days(1))
                && request.generated_end == Some(start - Duration::nanoseconds(1))
                && request.station_ids == "KORD"
                && stations == &["KORD"]
        })
        .times(2)
        .returning(|_, _| {
            let mut forecasts = mock_forecast_data();
            forecasts[0].date = "2024-08-12 00:00:00".into();
            forecasts[0].temp_high = 81;
            forecasts[0].temp_low = 62;
            // The adjacent day arrives last, so an unfiltered station map
            // would overwrite the requested day's forecast with this row.
            forecasts[1].date = "2024-08-13 00:00:00".into();
            forecasts[1].temp_high = 98;
            forecasts[1].temp_low = 42;
            Ok(forecasts)
        });
    weather
        .expect_stations()
        .times(1)
        .returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;

    // The selected calendar day is expressed with an offset. Both the
    // query and refresh must preserve its UTC day, not the supplied offset.
    let (status, body) = app
        .get("/fragments/weather?stations=KORD&start=2024-08-12T02:00:00%2B02:00&end=2024-08-13T02:00:00%2B02:00")
        .await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Selected period (UTC)"));
    assert!(html.contains("81°F"));
    assert!(html.contains("62°F"));
    assert!(html.contains("+6°F"));
    assert!(html.contains("+7°F"));
    assert!(!html.contains("98°F"));
    assert!(!html.contains("42°F"));

    let refresh = weather_refresh_url(&html);
    assert_eq!(
        refresh,
        "/fragments/weather?stations=KORD&start=2024-08-12T00%3A00%3A00Z&end=2024-08-13T00%3A00%3A00Z"
    );
    let (status, body) = app.get(&refresh).await;
    assert!(status.is_success());
    let refreshed = String::from_utf8(body.to_vec()).unwrap();
    assert!(refreshed.contains("81°F"));
    assert!(refreshed.contains("Selected period (UTC)"));
    assert_eq!(weather_refresh_url(&refreshed), refresh);
}

#[tokio::test]
async fn start_only_weather_selection_keeps_its_bound_and_refresh_context() {
    let start = OffsetDateTime::parse("2024-08-12T00:00:00Z", &Rfc3339).unwrap();
    let requested_ends = Arc::new(Mutex::new(Vec::new()));
    let captured_ends = requested_ends.clone();
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_observation_data()
        .withf(move |request, stations| {
            request.start == Some(start) && request.station_ids == "KORD" && stations == &["KORD"]
        })
        .times(2)
        .returning(move |request, _| {
            captured_ends.lock().unwrap().push(request.end.unwrap());
            Ok(mock_observation_data())
        });
    weather.expect_forecasts_data().never();
    weather
        .expect_stations()
        .times(1)
        .returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;
    let before = OffsetDateTime::now_utc();
    let (status, body) = app
        .get("/fragments/weather?stations=KORD&start=2024-08-12T02:00:00%2B02:00")
        .await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Selected period (UTC)"));
    assert!(!html.contains("Today so far (UTC)"));
    let refresh = weather_refresh_url(&html);
    assert_eq!(
        refresh,
        "/fragments/weather?stations=KORD&start=2024-08-12T00%3A00%3A00Z"
    );
    let (status, body) = app.get(&refresh).await;
    assert!(status.is_success());
    let refreshed = String::from_utf8(body.to_vec()).unwrap();
    assert!(refreshed.contains("Selected period (UTC)"));
    assert_eq!(weather_refresh_url(&refreshed), refresh);
    let after = OffsetDateTime::now_utc();
    let ends = requested_ends.lock().unwrap();
    assert_eq!(
        ends.len(),
        2,
        "selected windows do not fetch a second recent-report window"
    );
    assert!(ends.iter().all(|end| *end >= before && *end <= after));
}

#[tokio::test]
async fn multi_day_selection_does_not_compare_against_a_single_day_forecast() {
    let start = OffsetDateTime::parse("2024-08-12T00:00:00Z", &Rfc3339).unwrap();
    let end = start + Duration::days(2);
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_observation_data()
        .withf(move |request, stations| {
            request.start == Some(start) && request.end == Some(end) && stations == &["KORD"]
        })
        .times(1)
        .returning(|_, _| Ok(mock_observation_data()));
    weather.expect_forecasts_data().never();
    weather
        .expect_stations()
        .times(1)
        .returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;

    let (status, body) = app
        .get("/fragments/weather?stations=KORD&start=2024-08-12T00:00:00Z&end=2024-08-14T00:00:00Z")
        .await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Selected period (UTC)"));
    assert!(html.contains("75°F"));
    assert!(html.contains("55°F"));
    assert!(!html.contains("+0°F"));
    let forecast_cell = html
        .split_once("weather-forecast-cell\">")
        .unwrap()
        .1
        .split_once("</td>")
        .unwrap()
        .0;
    assert!(!forecast_cell.contains("°F"));
    assert!(forecast_cell.contains("—"));
}

#[tokio::test]
async fn latest_report_remains_visible_before_the_first_observation_of_today() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_observation_data()
        .withf(|request, _| {
            request
                .start
                .is_some_and(|start| start.time() == Time::MIDNIGHT)
                && request
                    .start
                    .zip(request.end)
                    .is_some_and(|(start, end)| start.date() == end.date())
        })
        .times(1)
        .returning(|_, _| Ok(vec![]));
    weather
        .expect_observation_data()
        .withf(|request, _| {
            request
                .start
                .zip(request.end)
                .is_some_and(|(start, end)| end - start == Duration::hours(24))
        })
        .times(1)
        .returning(|request, _| {
            let timestamp =
                request.end.unwrap().replace_time(Time::MIDNIGHT) - Duration::nanoseconds(1);
            let mut observations = mock_observation_data();
            observations[0].latest_temp_time = Some(timestamp.format(&Rfc3339).unwrap());
            Ok(observations)
        });
    weather
        .expect_forecasts_data()
        .times(1)
        .returning(|_, _| Ok(vec![]));
    weather
        .expect_stations()
        .times(1)
        .returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;

    let (status, body) = app.get("/fragments/weather?stations=KORD").await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("KORD"));
    assert!(html.contains("63°F"));
    assert!(html.contains("Latest observed"));
    assert!(html.contains("Today so far (UTC)"));
    assert!(html.contains("23:59:59.999999999Z"));
    assert!(!html.contains("75°F"));
    assert!(!html.contains("55°F"));
    assert!(!html.contains("10 kt"));
    assert!(!html.contains("No weather data available"));
    let row = html
        .split_once("weather-row\"")
        .unwrap()
        .1
        .split_once("</tr>")
        .unwrap()
        .0;
    assert!(row.matches(">—</span>").count() >= 7);
    assert_eq!(
        weather_refresh_url(&html),
        "/fragments/weather?stations=KORD"
    );
}

fn weather_refresh_url(html: &str) -> String {
    html.split_once("id=\"weather-table-container\"")
        .unwrap()
        .1
        .split_once("hx-get=\"")
        .unwrap()
        .1
        .split_once('"')
        .unwrap()
        .0
        .replace("&amp;", "&")
}

#[tokio::test]
async fn dashboard_refresh_retains_requested_stations_without_reports() {
    assert_dashboard_selection_survives_refresh(true).await;
}

#[tokio::test]
async fn empty_dashboard_retains_station_selection_and_dates_on_refresh() {
    assert_dashboard_selection_survives_refresh(false).await;
}

async fn assert_dashboard_selection_survives_refresh(has_observations: bool) {
    let start = OffsetDateTime::parse("2024-08-12T00:00:00Z", &Rfc3339).unwrap();
    let end = start + Duration::days(2);
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_observation_data()
        .withf(move |request, stations| {
            request.start == Some(start)
                && request.end == Some(end)
                && request.station_ids == "KORD,KBOS"
                && stations == &["KORD", "KBOS"]
        })
        .times(2)
        .returning(move |_, _| {
            Ok(if has_observations {
                mock_observation_data()
            } else {
                vec![]
            })
        });
    weather.expect_forecasts_data().never();
    weather.expect_stations().times(1).returning(|| {
        let mut stations = mock_stations();
        stations.push(Station {
            station_id: "KBOS".into(),
            station_name: "Boston Logan".into(),
            state: "MA".into(),
            iata_id: "BOS".into(),
            elevation_m: Some(6.0),
            latitude: 42.36,
            longitude: -71.01,
        });
        Ok(stations)
    });
    let app = spawn_app(Arc::new(weather)).await;
    let (status, body) = app
        .get("/?stations=KORD%2CKBOS&start=2024-08-12T02:00:00%2B02:00&end=2024-08-14T02:00:00%2B02:00")
        .await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(
        html.contains("No weather data available"),
        !has_observations
    );
    // Available stations remain selectable even when no requested reports exist.
    assert!(html.contains("hx-get=\"/fragments/weather?add_station=KBOS\""));
    let refresh = weather_refresh_url(&html);
    assert_eq!(
        refresh,
        "/fragments/weather?stations=KORD%2CKBOS&start=2024-08-12T00%3A00%3A00Z&end=2024-08-14T00%3A00%3A00Z"
    );
    let (status, body) = app.get(&refresh).await;
    assert!(status.is_success());
    let refreshed = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(weather_refresh_url(&refreshed), refresh);
    assert_eq!(
        refreshed.contains("No weather data available"),
        !has_observations
    );
}

/// Test that the forecast fragment endpoint returns forecast data
#[tokio::test]
async fn forecast_fragment_returns_forecast_data() {
    let mut weather_data = MockWeatherAccess::new();

    // Handler calls forecasts_data twice: once for future, once for past
    weather_data
        .expect_forecasts_data()
        .withf(|req, station_ids| {
            // Should request forecasts starting from now
            req.start.is_some() && req.end.is_some() && station_ids.contains(&"KORD".to_string())
        })
        .times(2)
        .returning(|_, _| Ok(mock_forecast_data()));

    // Handler also calls daily_observations for comparison data
    weather_data
        .expect_daily_observations()
        .times(1)
        .returning(|_, _| Ok(vec![]));

    weather_data
        .expect_stations()
        .returning(|| Ok(mock_stations()));
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
    assert!(html.contains("Forecasts and observations for KORD"));
    // Should contain temperature values from our mock data
    assert!(html.contains("75"));
    assert!(html.contains("55"));
}

/// Test that forecast fragment handles missing data gracefully
#[tokio::test]
async fn forecast_fragment_handles_no_data() {
    let mut weather_data = MockWeatherAccess::new();

    // Handler calls forecasts_data twice: once for future, once for past
    weather_data
        .expect_forecasts_data()
        .times(2)
        .returning(|_, _| Ok(vec![]));

    // Handler also calls daily_observations for comparison data
    weather_data
        .expect_daily_observations()
        .times(1)
        .returning(|_, _| Ok(vec![]));

    weather_data
        .expect_stations()
        .returning(|| Ok(mock_stations()));
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

    // The default airports first, then every station as a fallback.
    weather_data
        .expect_observation_data()
        .times(4)
        .returning(|_, _| Ok(vec![]));

    weather_data
        .expect_forecasts_data()
        .times(2)
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

/// The default dashboard names its stations: a forecast query over every
/// station exceeds the per-query memory limit and left forecasts empty.
#[tokio::test]
async fn dashboard_queries_forecasts_for_default_airports() {
    let mut weather_data = MockWeatherAccess::new();

    weather_data
        .expect_observation_data()
        .withf(|_, stations| stations.iter().any(|station| station == "KORD"))
        .times(2)
        .returning(|_, _| Ok(mock_observation_data()));

    weather_data
        .expect_forecasts_data()
        .withf(|_, stations| stations.iter().any(|station| station == "KORD"))
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
    assert!(html.contains("KORD"));
}

fn mock_observation_data() -> Vec<Observation> {
    vec![Observation {
        station_id: String::from("KORD"),
        start_time: String::from("2024-08-12T00:00:00+00:00"),
        end_time: String::from("2024-08-12T23:59:59+00:00"),
        latest_temp: Some(63.0),
        latest_temp_time: Some("2024-08-12T23:53:00Z".into()),
        temp_low: 55.0,
        temp_high: 75.0,
        wind_speed: Some(10),
        temp_unit_code: TemperatureUnit::Fahrenheit.to_string(),
        wind_direction: None,
        humidity: None,
        rain_amt: None,
        snow_amt: None,
        ice_amt: None,
    }]
}

fn mock_forecast_data() -> Vec<Forecast> {
    vec![
        Forecast {
            station_id: String::from("KORD"),
            date: time::OffsetDateTime::now_utc().date().to_string(),
            start_time: String::from("2024-08-13T00:00:00+00:00"),
            end_time: String::from("2024-08-14T00:00:00+00:00"),
            temp_low: 55,
            temp_high: 75,
            wind_speed: Some(12),
            wind_direction: None,
            humidity_max: None,
            humidity_min: None,
            temp_unit_code: TemperatureUnit::Fahrenheit.to_string(),
            precip_chance: None,
            rain_amt: None,
            snow_amt: None,
            ice_amt: None,
        },
        Forecast {
            station_id: String::from("KORD"),
            date: (time::OffsetDateTime::now_utc().date() + time::Duration::days(1)).to_string(),
            start_time: String::from("2024-08-14T00:00:00+00:00"),
            end_time: String::from("2024-08-15T00:00:00+00:00"),
            temp_low: 58,
            temp_high: 78,
            wind_speed: Some(8),
            wind_direction: None,
            humidity_max: None,
            humidity_min: None,
            temp_unit_code: TemperatureUnit::Fahrenheit.to_string(),
            precip_chance: None,
            rain_amt: None,
            snow_amt: None,
            ice_amt: None,
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

#[tokio::test]
async fn invalid_or_unbounded_weather_requests_are_rejected() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    for path in [
        "/fragments/forecast/KORD%27%29%3Balert(1)",
        "/stations/observations?station_ids=",
        "/stations/observations?station_ids=KORD&start=2020-01-01T00:00:00Z&end=2021-01-01T00:00:00Z",
        "/stations/forecasts?station_ids=bad%27id",
        "/files?start=2020-01-01T00:00:00Z&end=2026-01-01T00:00:00Z",
    ] {
        let (status, _) = test_app.get(path).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{path}");
    }
}

fn expect_current_observations(weather: &mut MockWeatherAccess) {
    weather
        .expect_observation_data()
        .withf(|request, _| {
            request
                .start
                .is_some_and(|start| start.time() == time::Time::MIDNIGHT)
                && request
                    .end
                    .is_some_and(|end| Some(end.date()) == request.start.map(|start| start.date()))
        })
        .times(1)
        .returning(|_, _| Ok(mock_observation_data()));
    weather
        .expect_observation_data()
        .withf(|request, _| {
            request
                .start
                .zip(request.end)
                .is_some_and(|(start, end)| end - start == time::Duration::hours(24))
        })
        .times(1)
        .returning(|_, _| Ok(mock_observation_data()));
}

#[tokio::test]
async fn empty_weather_refresh_retains_requested_stations_and_selected_dates() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_observation_data()
        .times(1)
        .returning(|_, _| Ok(vec![]));
    weather
        .expect_forecasts_data()
        .times(1)
        .returning(|_, _| Ok(vec![]));
    weather
        .expect_stations()
        .times(1)
        .returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;
    let (status, body) = app.get("/fragments/weather?stations=KORD,KBOS&start=2024-08-12T00:00:00Z&end=2024-08-13T00:00:00Z").await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("No weather data available"));
    assert_eq!(
        weather_refresh_url(&html),
        "/fragments/weather?stations=KORD%2CKBOS&start=2024-08-12T00%3A00%3A00Z&end=2024-08-13T00%3A00%3A00Z"
    );
}
