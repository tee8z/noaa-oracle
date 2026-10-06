use crate::helpers::{MockWeatherAccess, spawn_app};
use axum::http::StatusCode;
use oracle::weather_data::{Eligibility, Error};
use serde_json::{Value, json};
use std::sync::Arc;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339, macros::offset};

fn window() -> (OffsetDateTime, OffsetDateTime, String) {
    let start = OffsetDateTime::now_utc() + Duration::hours(2);
    let end = start + Duration::DAY;
    let url = format!(
        "/stations/eligible/forecasts?days=3&start={}&end={}",
        start.format(&Rfc3339).unwrap(),
        end.format(&Rfc3339).unwrap()
    );
    (start, end, url)
}

fn fixture(weather: &mut MockWeatherAccess, end: OffsetDateTime, count: usize) {
    weather
        .expect_eligible_stations()
        .times(1)
        .returning(move |days, hours, now| {
            assert_eq!((days, hours), (3, 24));
            Ok((0..count)
                .map(|i| Eligibility {
                    station_id: format!("S{i:04}"),
                    clean_days: 3,
                    days_checked: 3,
                    last_report: now,
                    forecast_through: Some(if i == 1 { end - Duration::HOUR } else { end }),
                    eligible: i != 2,
                })
                .collect())
        });
    weather.expect_stations().times(1).returning(move || Ok((0..count).map(|i| {
        serde_json::from_value(json!({"station_id":format!("S{i:04}"),"station_name":"Station", "state":"XX", "iata_id":"", "latitude":40.0, "longitude":-80.0})).unwrap()
    }).collect()));
}

fn forecast(id: &str, start: OffsetDateTime, end: OffsetDateTime) -> oracle::Forecast {
    serde_json::from_value(json!({"station_id":id,"date":start.date().to_string(),
        "start_time":start.format(&Rfc3339).unwrap(),"end_time":end.format(&Rfc3339).unwrap(),
        "temp_low":40,"temp_high":65,"temp_unit_code":"fahrenheit"}))
    .unwrap()
}

#[tokio::test]
async fn discovery_queries_all_eligible_stations_once_and_preserves_missing_values() {
    let (start, end, url) = window();
    let mut weather = MockWeatherAccess::new();
    fixture(&mut weather, end, 225);
    weather
        .expect_forecasts_data()
        .times(1)
        .returning(move |request, ids| {
            assert_eq!(ids.len(), 223);
            assert!(!ids.contains(&"S0001".into()));
            assert!(!ids.contains(&"S0002".into()));
            assert_eq!((request.start, request.end), (Some(start), Some(end)));
            assert_eq!(
                request.temperature_unit,
                oracle::TemperatureUnit::Fahrenheit
            );
            Ok(vec![
                forecast("S0000", start, end),
                forecast("S0002", start, end),
            ])
        });
    let app = spawn_app(Arc::new(weather)).await;
    let result: Value = app.get_json(&url).await;
    assert_eq!(result["stations"].as_array().unwrap().len(), 223);
    assert_eq!(result["forecasts"].as_array().unwrap().len(), 1);
    assert_eq!(result["forecasts"][0]["station_id"], "S0000");
    assert!(result["forecasts"][0]["wind_speed"].is_null());
    assert!(result["forecasts"][0]["precip_chance"].is_null());
}

#[tokio::test]
async fn empty_eligibility_never_scans_unfiltered_forecasts() {
    let (_, end, url) = window();
    let mut weather = MockWeatherAccess::new();
    fixture(&mut weather, end, 0);
    weather.expect_forecasts_data().never();
    let app = spawn_app(Arc::new(weather)).await;
    let result: Value = app.get_json(&url).await;
    assert_eq!(result, json!({"stations":[],"forecasts":[]}));
}

#[tokio::test]
async fn forecast_failure_does_not_publish_partial_discovery() {
    let (_, end, url) = window();
    let mut weather = MockWeatherAccess::new();
    fixture(&mut weather, end, 3);
    weather
        .expect_forecasts_data()
        .times(1)
        .returning(|_, _| Err(Error::QualityUnavailable));
    let app = spawn_app(Arc::new(weather)).await;
    let (status, body) = app.get(&url).await;
    assert!(status.is_server_error());
    assert!(!String::from_utf8_lossy(&body).contains("S0000"));
}

#[tokio::test]
async fn invalid_discovery_queries_do_not_touch_weather() {
    let (start, _, url) = window();
    let app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    for invalid in [
        url.replace("days=3", "days=32"),
        // Longer histories read weeks of reports: operators only.
        url.replace("days=3", "days=4"),
        url.replace("days=3", "days=30"),
        format!("{url}&station_ids=S0000"),
        format!(
            "/stations/eligible/forecasts?start={}&end={}",
            start.format(&Rfc3339).unwrap(),
            (start + Duration::hours(49)).format(&Rfc3339).unwrap()
        ),
    ] {
        let (status, _) = app.get(&invalid).await;
        assert!(matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
        ));
    }
}

/// The coordinator asks the same question every few minutes: one build
/// answers them all until new data arrives.
#[tokio::test]
async fn repeated_discovery_questions_are_answered_from_one_build() {
    let (start, end, url) = window();
    let mut weather = MockWeatherAccess::new();
    fixture(&mut weather, end, 3);
    weather
        .expect_forecasts_data()
        .times(1)
        .returning(move |_, _| Ok(vec![forecast("S0000", start, end)]));
    let app = spawn_app(Arc::new(weather)).await;
    let first: Value = app.get_json(&url).await;
    assert_eq!(first["stations"].as_array().unwrap().len(), 1);
    // The same instants written with another offset are the same question.
    let eastern = format!(
        "/stations/eligible/forecasts?days=3&start={}&end={}",
        start.to_offset(offset!(-4)).format(&Rfc3339).unwrap(),
        end.to_offset(offset!(-4)).format(&Rfc3339).unwrap()
    );
    for path in [url.clone(), url, eastern] {
        let again: Value = app.get_json(&path).await;
        assert_eq!(again, first, "{path}");
    }
    let metrics = app.metrics().await;
    assert_eq!(
        crate::helpers::metric(&metrics, r#"oracle_cache_entries{cache="discovery"}"#),
        1
    );
}

/// Operators may judge weeks of history; the public listener refuses them.
#[tokio::test]
async fn operators_may_ask_for_longer_histories() {
    let (start, end, url) = window();
    let url = url.replace("days=3", "days=14");
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_eligible_stations()
        .times(1)
        .returning(move |days, hours, now| {
            assert_eq!((days, hours), (14, 24));
            Ok(vec![Eligibility {
                station_id: "S0000".into(),
                clean_days: 14,
                days_checked: 14,
                last_report: now,
                forecast_through: Some(end),
                eligible: true,
            }])
        });
    weather.expect_stations().times(1).returning(|| {
        Ok(vec![
            serde_json::from_value(json!({"station_id":"S0000","station_name":"Station",
            "state":"XX","iata_id":"","latitude":40.0,"longitude":-80.0}))
            .unwrap(),
        ])
    });
    weather
        .expect_forecasts_data()
        .times(1)
        .returning(move |_, _| Ok(vec![forecast("S0000", start, end)]));
    let app = spawn_app(Arc::new(weather)).await;
    let (status, body) = app.get(&url).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        String::from_utf8_lossy(&body).contains("1–3 history days"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (status, body) = app.get_operator(&url).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let result: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["stations"][0]["days_checked"], 14);
    assert_eq!(result["forecasts"].as_array().unwrap().len(), 1);
    // Kept for the next operator, and still refused publicly.
    let (status, _) = app.get_operator(&url).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app.get(&url).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// While heavy work holds every turn and the line for one is full, a
/// question nobody has asked yet is turned away at once with a time to
/// retry; questions already answered are still answered.
#[tokio::test]
async fn new_questions_are_turned_away_while_heavy_work_is_busy() {
    let (start, end, url) = window();
    let mut weather = MockWeatherAccess::new();
    fixture(&mut weather, end, 3);
    weather
        .expect_forecasts_data()
        .times(2)
        .returning(move |_, _| Ok(vec![forecast("S0000", start, end)]));
    let app = spawn_app(Arc::new(weather)).await;
    let kept: Value = app.get_json(&url).await;

    let heavy = app.state.heavy().clone();
    let pass = heavy
        .every_turn(std::time::Duration::ZERO)
        .await
        .expect("every turn");
    let waiting: Vec<_> = (0..4)
        .map(|_| {
            let heavy = heavy.clone();
            tokio::spawn(async move { heavy.turn().await.is_some() })
        })
        .collect();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while heavy.waiting() < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the line for a turn fills");

    let again: Value = app.get_json(&url).await;
    assert_eq!(again, kept);
    // An hour earlier: the kept eligible list serves it, the answer is new.
    let earlier = format!(
        "/stations/eligible/forecasts?days=3&start={}&end={}",
        (start - Duration::HOUR).format(&Rfc3339).unwrap(),
        (end - Duration::HOUR).format(&Rfc3339).unwrap()
    );
    let request = axum::http::Request::get(&earlier)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(app.app.clone(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "5");
    let metrics = app.metrics().await;
    assert_eq!(
        crate::helpers::metric(&metrics, "oracle_heavy_requests_turned_away_total"),
        1
    );

    for task in waiting {
        task.abort();
    }
    drop(pass);
    let built: Value = app.get_json(&earlier).await;
    assert_eq!(built["stations"].as_array().unwrap().len(), 2);
}
