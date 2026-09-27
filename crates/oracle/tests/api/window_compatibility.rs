use crate::helpers::{MockWeatherAccess, spawn_app};
use axum::http::StatusCode;
use oracle::weather_data::{Error, ForecastAssessment, ForecastNativeInterval};
use serde_json::Value;
use std::sync::Arc;
use time::{Duration, macros::datetime};

const WINDOW: &str = "start=2026-09-28T00:00:00Z&end=2026-09-28T06:00:00Z";

fn value(metric: &str, amount: Option<f64>) -> ForecastAssessment {
    ForecastAssessment {
        station_id: "KORD".into(),
        metric: metric.into(),
        value: amount,
        reason: None,
        native_intervals: vec![],
    }
}

#[tokio::test]
async fn planning_exposes_native_windows_and_reasons_without_promising_settlement() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_forecast_assessment()
        .times(1)
        .returning(|request, stations| {
            assert_eq!(request.start, Some(datetime!(2026-09-28 00:00 UTC)));
            assert_eq!(request.end, Some(datetime!(2026-09-28 06:00 UTC)));
            assert_eq!(request.generated_end, Some(datetime!(2026-09-27 00:00 UTC)));
            assert_eq!(
                request.generated_start,
                request.generated_end.map(|end| end - Duration::days(7))
            );
            assert_eq!(stations, ["KORD", "KSAW"]);
            assert_eq!(
                request.temperature_unit,
                oracle::TemperatureUnit::Fahrenheit
            );
            Ok(vec![
                ForecastAssessment {
                    station_id: "KORD".into(),
                    metric: "temp_high".into(),
                    value: None,
                    reason: Some(
                        "requested window does not contain a native maximum-temperature period"
                            .into(),
                    ),
                    native_intervals: vec![ForecastNativeInterval {
                        metric: "max_temp".into(),
                        start: "2026-09-28T06:00:00Z".into(),
                        end: Some("2026-09-28T18:00:00Z".into()),
                    }],
                },
                value("rain_amt", Some(0.2)),
            ])
        });
    weather
        .expect_precipitation_station_capabilities()
        .times(1)
        .returning(|_| Ok(vec!["KORD".into()]));
    weather.expect_settlement_observations().never();
    weather.expect_observation_data().never();
    let app = spawn_app(Arc::new(weather)).await;
    app.clock.set(datetime!(2026-09-27 00:00 UTC));
    let response: Value = app.get_json(&format!(
        "/stations/window-compatibility?{WINDOW}&station_ids=KORD,KSAW&metrics=temp_high,rain_amt"
    )).await;
    assert_eq!(response["evaluated_at"], "2026-09-27T00:00:00Z");
    assert_eq!(response["requested_window"]["end"], "2026-09-28T06:00:00Z");
    assert_eq!(response["observations"], "pending");
    assert_eq!(response["settlement_ready"], false);
    assert_eq!(response["forecasts"].as_array().unwrap().len(), 4);
    assert_eq!(response["forecasts"][0]["baseline_available"], false);
    assert!(
        response["forecasts"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("native maximum")
    );
    assert_eq!(
        response["forecasts"][0]["native_intervals"][0]["end"],
        "2026-09-28T18:00:00Z"
    );
    assert_eq!(response["forecasts"][1]["baseline_available"], true);
    assert_eq!(response["forecasts"][1]["baseline"], 0.2);
    assert_eq!(response["forecasts"][1]["unit"], "inches");
    assert_eq!(response["forecasts"][2]["baseline_available"], false);
    assert_eq!(
        response["stations"][0]["fixed_hour_precipitation_source"],
        "recently_observed"
    );
    assert_eq!(
        response["stations"][1]["fixed_hour_precipitation_source"],
        "unknown"
    );
    let events: Value = app.get_json("/oracle/events").await;
    assert_eq!(
        events,
        serde_json::json!([]),
        "planning must not create an event"
    );
}

#[tokio::test]
async fn invalid_or_unbounded_plans_fail_before_reading_the_source() {
    let mut weather = MockWeatherAccess::new();
    weather.expect_forecast_assessment().never();
    weather.expect_precipitation_station_capabilities().never();
    let app = spawn_app(Arc::new(weather)).await;
    let too_many = (0..21)
        .map(|index| format!("K{index:03}"))
        .collect::<Vec<_>>()
        .join(",");
    let queries = [
        format!("{WINDOW}&station_ids="),
        format!("{WINDOW}&station_ids=KORD,KORD"),
        format!("{WINDOW}&station_ids=KORD,,KSAW"),
        format!("{WINDOW}&station_ids={too_many}"),
        format!("{WINDOW}&station_ids=KORD%27"),
        format!("{WINDOW}&station_ids=KORD&metrics=rain_amt,rain_amt"),
        format!("{WINDOW}&station_ids=KORD&metrics=temperature"),
        format!("{WINDOW}&station_ids=KORD&metrics="),
        format!("{WINDOW}&station_ids=KORD&temperature_unit=celsius"),
        "start=2026-09-28T00:00:00Z&end=2026-10-06T00:00:00Z&station_ids=KORD".into(),
        "start=2026-09-28T00:00:00Z&end=2026-09-28T00:00:00Z&station_ids=KORD".into(),
        "end=2026-09-28T00:00:00Z&station_ids=KORD".into(),
    ];
    for query in queries {
        let (status, body) = app
            .get(&format!("/stations/window-compatibility?{query}"))
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{query}: {}",
            String::from_utf8_lossy(&body)
        );
    }
}

#[tokio::test]
async fn an_unavailable_assessment_cannot_look_like_a_compatible_empty_window() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_forecast_assessment()
        .returning(|_, _| Err(Error::QualityUnavailable));
    weather
        .expect_precipitation_station_capabilities()
        .returning(|_| Ok(vec![]));
    let app = spawn_app(Arc::new(weather)).await;
    let (status, _) = app
        .get(&format!(
            "/stations/window-compatibility?{WINDOW}&station_ids=KORD"
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn past_windows_and_unknown_precipitation_support_never_claim_observation_readiness() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_forecast_assessment()
        .returning(|_, _| Ok(vec![value("wind_speed", Some(10.0))]));
    weather
        .expect_precipitation_station_capabilities()
        .returning(|_| Err(Error::QualityUnavailable));
    let app = spawn_app(Arc::new(weather)).await;
    app.clock.set(datetime!(2026-10-01 00:00 UTC));
    let response: Value = app
        .get_json(&format!(
            "/stations/window-compatibility?{WINDOW}&station_ids=KORD&metrics=wind_speed"
        ))
        .await;
    assert_eq!(response["forecasts"][0]["baseline_available"], true);
    assert_eq!(response["observations"], "not_assessed");
    assert_eq!(response["settlement_ready"], false);
    assert_eq!(
        response["stations"][0]["fixed_hour_precipitation_source"],
        "unknown"
    );
    assert!(response["precipitation_capability_warning"].is_string());
}

#[tokio::test]
async fn ambiguous_or_nonfinite_forecasts_are_explicitly_unavailable() {
    let mut weather = MockWeatherAccess::new();
    weather.expect_forecast_assessment().returning(|_, _| {
        Ok(vec![
            value("temp_high", Some(70.0)),
            value("temp_high", Some(71.0)),
            value("temp_low", Some(f64::NAN)),
        ])
    });
    weather
        .expect_precipitation_station_capabilities()
        .returning(|_| Ok(vec![]));
    let app = spawn_app(Arc::new(weather)).await;
    let response: Value = app
        .get_json(&format!(
            "/stations/window-compatibility?{WINDOW}&station_ids=KORD"
        ))
        .await;
    assert_eq!(response["forecasts"].as_array().unwrap().len(), 3);
    for forecast in response["forecasts"].as_array().unwrap() {
        assert_eq!(forecast["baseline_available"], false);
        assert!(forecast["baseline"].is_null());
        assert!(forecast["reason"].is_string());
    }
}
