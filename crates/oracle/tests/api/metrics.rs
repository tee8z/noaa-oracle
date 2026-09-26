//! The metrics listener serves only `GET /metrics`, and the public router
//! never serves metrics.

use crate::helpers::{MockWeatherAccess, metric, signed, spawn_app};
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header::CONTENT_TYPE},
};
use std::sync::Arc;
use time::macros::datetime;
use tower::ServiceExt;

const FORECAST: &str = "forecasts_2030-01-02T06:00:00Z.parquet";
const OBSERVATION: &str = "observations_2030-01-01T00:00:00Z.parquet";

#[tokio::test]
async fn the_metrics_router_serves_every_family_at_get_metrics_only() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    for name in [FORECAST, OBSERVATION] {
        let request = signed(
            Method::POST,
            &format!("/file/{name}"),
            b"PAR1rowsPAR1".to_vec(),
            &test_app.uploader,
        );
        assert_eq!(test_app.send(request).await.0, StatusCode::CREATED);
    }
    test_app.state.wait_for_etl().await;

    let router = oracle::metrics::router(test_app.state.clone());
    let response = router
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    let text = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    for family in [
        "oracle_build_info",
        "oracle_events",
        "oracle_events_awaiting_attestation",
        "oracle_oldest_event_awaiting_attestation_age_seconds",
        "oracle_etl_runs_total",
        "oracle_events_attested_total",
        "oracle_event_attestation_failures_total",
        "oracle_last_etl_completed_timestamp_seconds",
        "oracle_etl_lease_held",
        "oracle_uploads_total",
        "oracle_latest_forecast_timestamp_seconds",
        "oracle_latest_observation_timestamp_seconds",
    ] {
        assert!(text.contains(&format!("# TYPE {family} ")), "{family}");
    }
    let version = format!(
        r#"oracle_build_info{{version="{}"}}"#,
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(metric(&text, &version), 1);
    assert_eq!(
        metric(&text, r#"oracle_uploads_total{kind="forecasts"}"#),
        1
    );
    assert_eq!(
        metric(&text, r#"oracle_uploads_total{kind="observations"}"#),
        1
    );
    assert_eq!(
        metric(&text, "oracle_latest_forecast_timestamp_seconds"),
        datetime!(2030-01-02 06:00 UTC).unix_timestamp()
    );
    assert_eq!(
        metric(&text, "oracle_latest_observation_timestamp_seconds"),
        datetime!(2030-01-01 00:00 UTC).unix_timestamp()
    );
    for state in ["live", "running", "completed", "signed", "unlisted"] {
        assert_eq!(
            metric(&text, &format!(r#"oracle_events{{state="{state}"}}"#)),
            0
        );
    }
    // Each upload started a pass; with no events they complete at once.
    assert!(metric(&text, r#"oracle_etl_runs_total{result="completed"}"#) >= 1);

    let status = |method: Method, path: &str| {
        let router = router.clone();
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        async move { router.oneshot(request).await.unwrap().status() }
    };
    assert_eq!(status(Method::GET, "/").await, StatusCode::NOT_FOUND);
    assert_eq!(status(Method::GET, "/health").await, StatusCode::NOT_FOUND);
    assert_eq!(
        status(Method::POST, "/metrics").await,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn the_public_router_does_not_serve_metrics() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    let (status, body) = test_app.get("/metrics").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!String::from_utf8_lossy(&body).contains("oracle_"));
}
