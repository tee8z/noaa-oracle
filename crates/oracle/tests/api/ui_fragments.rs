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
        .uri("/?view=list")
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
    // The chosen view is remembered for the next visit.
    assert_eq!(
        response.headers()[header::SET_COOKIE],
        "weather_view=list; Path=/; Max-Age=31536000; SameSite=Lax"
    );
    let policy = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(policy.starts_with("script-src 'self';"), "{policy}");

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    // Trigger filters are JavaScript, which htmx may not evaluate here.
    for trigger in html.split("hx-trigger=\"").skip(1) {
        let trigger = trigger.split_once('"').unwrap().0;
        assert!(!trigger.contains('['), "{trigger}");
    }

    // Verify the response contains weather data
    assert!(html.contains("Current weather"));
    // Should contain our mock station
    assert!(html.contains("KORD"));
    assert!(html.contains("Latest"));
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
        .get("/fragments/weather?stations=KORD&start=2024-08-12T02:00:00%2B02:00&end=2024-08-13T02:00:00%2B02:00&view=list")
        .await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Selected period"));
    assert!(html.contains("81°F"));
    assert!(html.contains("62°F"));
    // Observed − forecast: 75 − 81 and 55 − 62.
    assert!(html.contains("-6°F"));
    assert!(html.contains("-7°F"));
    assert!(!html.contains("98°F"));
    assert!(!html.contains("42°F"));

    let refresh = weather_refresh_url(&html);
    assert_eq!(
        refresh,
        "/fragments/weather?stations=KORD&start=2024-08-12T00%3A00%3A00Z&end=2024-08-13T00%3A00%3A00Z&view=list"
    );
    let (status, body) = app.get(&refresh).await;
    assert!(status.is_success());
    let refreshed = String::from_utf8(body.to_vec()).unwrap();
    assert!(refreshed.contains("81°F"));
    assert!(refreshed.contains("-6°F"));
    assert!(refreshed.contains("Selected period"));
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
    assert!(html.contains("Selected period"));
    assert!(!html.contains("Today so far (UTC)"));
    let refresh = weather_refresh_url(&html);
    assert_eq!(
        refresh,
        "/fragments/weather?stations=KORD&start=2024-08-12T00%3A00%3A00Z&view=map"
    );
    let (status, body) = app.get(&refresh).await;
    assert!(status.is_success());
    let refreshed = String::from_utf8(body.to_vec()).unwrap();
    assert!(refreshed.contains("Selected period"));
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
        .get("/fragments/weather?stations=KORD&start=2024-08-12T00:00:00Z&end=2024-08-14T00:00:00Z&view=list")
        .await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Selected period"));
    assert!(html.contains("75°F"));
    assert!(html.contains("55°F"));
    assert!(!html.contains("+0°F"));
    let forecast_cell = html
        .split_once("class=\"wx-fcst\"><span class=\"cell-label\">Forecast </span>")
        .unwrap()
        .1
        .split_once("</span></span>")
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

    let (status, body) = app.get("/fragments/weather?stations=KORD&view=list").await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("KORD"));
    assert!(html.contains("63°F"));
    assert!(html.contains("Latest"));
    assert!(html.contains("Today so far (UTC)"));
    assert!(html.contains("23:59:59.999999999Z"));
    assert!(!html.contains("75°F"));
    assert!(!html.contains("55°F"));
    assert!(!html.contains("10 kt"));
    assert!(!html.contains("No weather data available"));
    let row = html
        .split_once("class=\"wx-row\"")
        .unwrap()
        .1
        .split_once("</summary>")
        .unwrap()
        .0;
    assert!(row.matches(">—</span>").count() >= 7);
    assert_eq!(
        weather_refresh_url(&html),
        "/fragments/weather?stations=KORD&view=list"
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
        .times(3)
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
    let refresh = weather_refresh_url(&html);
    assert_eq!(
        refresh,
        "/fragments/weather?stations=KORD%2CKBOS&start=2024-08-12T00%3A00%3A00Z&end=2024-08-14T00%3A00%3A00Z&view=map"
    );
    let (status, body) = app.get(&refresh).await;
    assert!(status.is_success());
    let refreshed = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(weather_refresh_url(&refreshed), refresh);
    assert_eq!(
        refreshed.contains("No weather data available"),
        !has_observations
    );

    // A search offers requested stations without reports, keeping the selection.
    let (status, body) = app
        .get(&format!(
            "{}&q=bos",
            refresh.replace("view=map", "view=list")
        ))
        .await;
    assert!(status.is_success());
    let searched = String::from_utf8(body.to_vec()).unwrap();
    assert!(searched.contains(
        "hx-get=\"/fragments/weather?stations=KORD%2CKBOS&amp;start=2024-08-12T00%3A00%3A00Z&amp;end=2024-08-14T00%3A00%3A00Z&amp;add_station=KBOS&amp;view=list\""
    ));
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

    // Should show a message about no data
    assert!(html.contains("No forecast data available"));
}

/// A failed forecast query is not shown, or cached, as "no data": the
/// reader gets an error with a retry, and the next request queries again.
#[tokio::test]
async fn failed_forecast_queries_offer_a_retry_and_are_not_cached() {
    let broken = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut weather = MockWeatherAccess::new();
    let failing = broken.clone();
    weather.expect_forecasts_data().returning(move |_, _| {
        if failing.load(std::sync::atomic::Ordering::SeqCst) {
            Err(oracle::weather_data::Error::InvalidStationId(
                "broken".into(),
            ))
        } else {
            Ok(mock_forecast_data())
        }
    });
    weather
        .expect_daily_observations()
        .returning(|_, _| Ok(vec![]));
    weather.expect_stations().returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;

    let (status, body) = app.get("/fragments/forecast/KORD").await;
    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Try again"), "{html}");
    assert!(
        html.contains("hx-get=\"/fragments/forecast/KORD\""),
        "{html}"
    );
    assert!(
        html.contains("hx-target=\"closest .wx-forecast\""),
        "{html}"
    );
    assert!(!html.contains("No forecast data available"), "{html}");

    let (status, body) = app.get("/fragments/station/KORD").await;
    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Chicago O"), "{html}");
    assert!(
        html.contains("hx-get=\"/fragments/station/KORD\""),
        "{html}"
    );
    assert!(html.contains("hx-target=\"#map-station\""), "{html}");

    broken.store(false, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = app.get("/fragments/forecast/KORD").await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("Forecasts and observations for KORD"),
        "{html}"
    );
}

/// Test that dashboard handles empty weather data gracefully
#[tokio::test]
async fn dashboard_handles_no_weather_data() {
    let mut weather_data = MockWeatherAccess::new();

    // The default airports first, then the first stations in the data;
    // never an empty list, which would query every station.
    weather_data
        .expect_observation_data()
        .withf(|_, stations| !stations.is_empty())
        .times(4)
        .returning(|_, _| Ok(vec![]));

    weather_data
        .expect_forecasts_data()
        .withf(|_, stations| !stations.is_empty())
        .times(2)
        .returning(|_, _| Ok(vec![]));

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

/// `?stations=` with nothing usable shows the default airports; it must
/// not query every station, which is what an empty list means to the data
/// layer. Every weather call here names the airports.
#[tokio::test]
async fn an_empty_station_list_shows_the_default_airports() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_observation_data()
        .withf(|_, stations| stations.len() > 1 && stations.iter().any(|id| id == "KORD"))
        .returning(|_, _| Ok(mock_observation_data()));
    weather
        .expect_forecasts_data()
        .withf(|_, stations| stations.len() > 1)
        .returning(|_, _| Ok(vec![]));
    weather.expect_stations().returning(|| Ok(vec![]));
    let app = spawn_app(Arc::new(weather)).await;
    for path in [
        "/?stations=",
        "/?stations=,%20,bad%27id",
        "/fragments/weather?stations=",
    ] {
        let (status, body) = app.get(path).await;
        assert!(status.is_success(), "{path}");
        assert!(
            String::from_utf8(body.to_vec()).unwrap().contains("KORD"),
            "{path}"
        );
    }
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
        "/fragments/weather?stations=KORD%2CKBOS&start=2024-08-12T00%3A00%3A00Z&end=2024-08-13T00%3A00%3A00Z&view=map"
    );
}

/// A search replaces only the list, so the search box keeps focus, and puts
/// the search in the address bar; a full load gets the whole page.
#[tokio::test]
async fn station_search_returns_only_the_list_to_htmx() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_observation_data()
        .returning(|_, _| Ok(mock_observation_data()));
    weather.expect_forecasts_data().returning(|_, _| Ok(vec![]));
    weather.expect_stations().returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;
    let path = "/fragments/weather?stations=KORD&view=list&q=chicago";

    let (_, response) = app
        .send(
            Request::get(path)
                .header("hx-request", "true")
                .header("hx-target", "div#weather-list")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let list = String::from_utf8(response.to_vec()).unwrap();
    assert!(list.starts_with("<div id=\"weather-list\""), "{list}");
    assert!(list.contains("KORD"));
    assert!(!list.contains("weather-search"));

    let response = app
        .app
        .clone()
        .oneshot(
            Request::get(path)
                .header("hx-request", "true")
                .header("hx-target", "div#weather-list")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers()["hx-replace-url"],
        "/?stations=KORD&view=list&q=chicago"
    );
    assert_eq!(
        response.headers()[header::VARY],
        "HX-Request, HX-Target, HX-History-Restore-Request, Cookie"
    );

    // Without a match the list says so.
    let (_, body) = app
        .send(
            Request::get("/fragments/weather?stations=KORD&view=list&q=boston")
                .header("hx-request", "true")
                .header("hx-target", "div#weather-list")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let empty = String::from_utf8(body.to_vec()).unwrap();
    assert!(empty.contains("No station in this list matches"));

    // A tab click or refresh gets the whole section, and a page load the page.
    let (_, body) = app
        .send(
            Request::get("/fragments/weather?stations=KORD&view=list")
                .header("hx-request", "true")
                .header("hx-target", "section#weather-table-container")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let section = String::from_utf8(body.to_vec()).unwrap();
    assert!(section.starts_with("<section id=\"weather-table-container\""));
    assert!(section.contains("weather-search"));
    let (_, body) = app.get("/?stations=KORD&view=list").await;
    assert!(
        String::from_utf8(body.to_vec())
            .unwrap()
            .starts_with("<!DOCTYPE html>")
    );
}

/// Map pins open a panel with the station's name and forecast detail.
#[tokio::test]
async fn map_station_panel_names_the_station() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_forecasts_data()
        .returning(|_, _| Ok(mock_forecast_data()));
    weather
        .expect_daily_observations()
        .returning(|_, _| Ok(vec![]));
    weather.expect_stations().returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;
    let (status, body) = app.get("/fragments/station/KORD").await;
    assert!(status.is_success());
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("Chicago O&#39;Hare, IL") || html.contains("Chicago O'Hare, IL"),
        "{html}"
    );
    assert!(html.contains("Forecasts and observations for KORD"));
    let (status, _) = app.get("/fragments/station/KORD%27x").await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

/// A well-formed id that no station has is not found, for the map panel
/// and a list row alike, and costs no forecast query.
#[tokio::test]
async fn unknown_stations_are_not_found() {
    let mut weather = MockWeatherAccess::new();
    weather.expect_forecasts_data().never();
    weather.expect_daily_observations().never();
    weather.expect_stations().returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;
    for path in ["/fragments/station/ZZZZ", "/fragments/forecast/ZZZZ"] {
        let (status, body) = app.get(path).await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{path}");
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("No station"), "{path}: {html}");
    }
}

/// A reader in New York gets their own day: it starts at their midnight,
/// and "yesterday's forecast" is the one issued in the day before it.
#[tokio::test]
async fn the_time_zone_cookie_sets_the_dashboards_day() {
    let new_york = oracle::calendar::Calendar::from_zone_name("America/New_York").unwrap();
    let midnight = new_york.start_of_day(OffsetDateTime::now_utc());
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_observation_data()
        .withf(move |request, _| request.start == Some(midnight))
        .times(1)
        .returning(|_, _| Ok(mock_observation_data()));
    weather
        .expect_observation_data()
        .withf(|request, _| {
            request
                .start
                .zip(request.end)
                .is_some_and(|(start, end)| end - start == Duration::hours(24))
        })
        .times(1)
        .returning(|_, _| Ok(mock_observation_data()));
    weather
        .expect_forecasts_data()
        .withf(move |request, _| {
            request.start == Some(midnight)
                && request.generated_end == Some(midnight - Duration::nanoseconds(1))
                && request.generated_start == Some(midnight - Duration::days(1))
        })
        .times(1)
        .returning(|_, _| Ok(vec![]));
    weather.expect_stations().returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;
    let request = Request::get("/fragments/weather?view=list")
        .header("HX-Request", "true")
        .header(header::COOKIE, "weather_view=map; tz=America/New_York")
        .body(Body::empty())
        .unwrap();
    let response = app.app.clone().oneshot(request).await.unwrap();
    assert!(response.status().is_success());
    let vary = response.headers()[header::VARY].to_str().unwrap();
    assert!(vary.contains("Cookie"), "{vary}");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Today so far (New York time)"), "{html}");
}

/// Forecast details depend on the reader's calendar: they say so to caches,
/// and each calendar gets its own cached copy.
#[tokio::test]
async fn forecast_details_vary_by_the_readers_calendar() {
    let mut weather = MockWeatherAccess::new();
    // Future and past forecasts, for UTC and then for New York; the second
    // request in each calendar comes from the cache.
    weather
        .expect_forecasts_data()
        .times(4)
        .returning(|_, _| Ok(mock_forecast_data()));
    weather
        .expect_daily_observations()
        .times(2)
        .returning(|_, _| Ok(vec![]));
    weather.expect_stations().returning(|| Ok(mock_stations()));
    let app = spawn_app(Arc::new(weather)).await;
    for cookie in [
        None,
        None,
        Some("tz=America/New_York"),
        Some("tz=America/New_York"),
    ] {
        for path in ["/fragments/forecast/KORD", "/fragments/station/KORD"] {
            let mut request = Request::get(path).header("HX-Request", "true");
            if let Some(cookie) = cookie {
                request = request.header(header::COOKIE, cookie);
            }
            let response = app
                .app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert!(response.status().is_success(), "{path}");
            assert_eq!(response.headers()[header::VARY], "Cookie", "{path}");
        }
    }
}
