//! What public requests may ask for, and what only the private listener,
//! which serves operators, answers.

use crate::helpers::{MockWeatherAccess, spawn_app};
use axum::http::StatusCode;
use std::sync::Arc;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

fn at(time: OffsetDateTime) -> String {
    time.format(&Rfc3339).unwrap()
}

/// Forecasts issued long ago are read from the published files, seconds
/// and a gigabyte or more a request: the public listener refuses them
/// before reading anything, operators still get them.
#[tokio::test]
async fn public_forecast_queries_read_only_recent_issues() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_forecasts_data()
        .times(1)
        .returning(|_, _| Ok(vec![]));
    let app = spawn_app(Arc::new(weather)).await;
    let now = OffsetDateTime::now_utc();
    let old = now - Duration::days(20);
    let past_window =
        "/stations/forecasts?station_ids=KORD&start=2020-01-01T00:00:00Z&end=2020-01-02T00:00:00Z";
    let old_issues = format!(
        "/stations/forecasts?station_ids=KORD&start={}&end={}&generated_start={}&generated_end={}",
        at(now),
        at(now + Duration::DAY),
        at(old),
        at(old + Duration::DAY)
    );
    for path in [past_window, old_issues.as_str()] {
        let (status, body) = app.get(path).await;
        let body = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert!(body.contains("last 10 days"), "{body}");
    }
    let (status, body) = app.get_operator(past_window).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
}

/// The queries the entry form, leaderboards and discovery make read recent
/// issues and are answered as before.
#[tokio::test]
async fn current_forecast_queries_are_answered() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_forecasts_data()
        .times(3)
        .returning(|_, _| Ok(vec![]));
    let app = spawn_app(Arc::new(weather)).await;
    let now = OffsetDateTime::now_utc();
    let start = now + Duration::hours(1);
    let started = now - Duration::days(2);
    for path in [
        // The entry form: tomorrow, the latest issues.
        format!(
            "/stations/forecasts?station_ids=KORD,KSAW&start={}&end={}",
            at(start),
            at(start + Duration::DAY)
        ),
        // Baselines: the week of issues before the window.
        format!(
            "/stations/forecasts?station_ids=KORD&start={}&end={}&generated_start={}&generated_end={}&temperature_unit=fahrenheit",
            at(started),
            at(started + Duration::DAY),
            at(started - Duration::days(7)),
            at(started - Duration::NANOSECOND)
        ),
        // No bounds: the coming week.
        "/stations/forecasts?station_ids=KORD".to_owned(),
    ] {
        let (status, body) = app.get(&path).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
    }
}
