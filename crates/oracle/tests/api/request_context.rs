//! Request ids, CORS headers and the telemetry endpoint.

use crate::helpers::{MockWeatherAccess, ORIGIN, spawn_app, spawn_app_with};
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode, header},
};
use oracle::request_context::{RequestSettings, is_request_id};
use std::{net::SocketAddr, sync::Arc};
use tower::ServiceExt;

const RID: &str = "0192f3a0-0000-7000-8000-0000000000aa";

fn from_proxy(peer: &str) -> Request<Body> {
    let mut request = Request::get("/healthy")
        .header("x-request-id", RID)
        .header("x-real-ip", "203.0.113.9")
        .body(Body::empty())
        .unwrap();
    let peer: SocketAddr = peer.parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    request
}

#[tokio::test]
async fn responses_echo_the_proxys_id_only_from_a_trusted_peer() {
    let settings = RequestSettings {
        trusted_proxies: vec!["127.0.0.1".parse().unwrap()],
        ..RequestSettings::default()
    };
    let app = spawn_app_with(Arc::new(MockWeatherAccess::new()), ORIGIN, settings).await;

    let response = app
        .app
        .clone()
        .oneshot(from_proxy("127.0.0.1:50000"))
        .await
        .unwrap();
    assert_eq!(response.headers()["x-request-id"], RID);

    let response = app
        .app
        .clone()
        .oneshot(from_proxy("198.51.100.7:50000"))
        .await
        .unwrap();
    let rid = response.headers()["x-request-id"].to_str().unwrap();
    assert_ne!(rid, RID);
    assert!(is_request_id(rid));

    // Not found and preflight answers carry an id too.
    let response = app
        .app
        .clone()
        .oneshot(Request::get("/nowhere").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(response.headers().contains_key("x-request-id"));
}

#[tokio::test]
async fn cors_allows_the_tracing_headers_and_exposes_the_id() {
    let app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    let response = app
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/oracle/events")
                .header(header::ORIGIN, "https://coordinator.example.com")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "x-session-id,x-parent-request-id",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
    let allowed = response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(allowed.contains("x-session-id"), "{allowed}");
    assert!(allowed.contains("x-parent-request-id"), "{allowed}");
    assert!(response.headers().contains_key("x-request-id"));

    let response = app
        .app
        .clone()
        .oneshot(
            Request::get("/healthy")
                .header(header::ORIGIN, "https://coordinator.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let exposed = response.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS]
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(exposed.contains("x-request-id"), "{exposed}");
}

fn beacon(content_type: &str, body: impl Into<Body>) -> Request<Body> {
    Request::post("/api/v1/telemetry")
        .header(header::CONTENT_TYPE, content_type)
        .body(body.into())
        .unwrap()
}

const BATCH: &str = r#"{"sid":"AbCdEfGhIjKlMnOpQrStUv","rid":null,"events":[{"ev":"page_view","t":10,"page":"/","ttfb":40}]}"#;

#[tokio::test]
async fn telemetry_is_off_by_default() {
    let app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    let (status, _) = app.send(beacon("application/json", BATCH)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn telemetry_accepts_batches_and_rejects_malformed_or_large_bodies() {
    let settings = RequestSettings {
        telemetry: true,
        ..RequestSettings::default()
    };
    let app = spawn_app_with(Arc::new(MockWeatherAccess::new()), ORIGIN, settings).await;
    for content_type in ["application/json", "text/plain;charset=UTF-8"] {
        let (status, _) = app.send(beacon(content_type, BATCH)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{content_type}");
    }
    let (status, _) = app.send(beacon("application/json", "{\"sid\":1}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = app
        .send(beacon("application/x-www-form-urlencoded", BATCH))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let large = format!(
        r#"{{"sid":"AbCdEfGhIjKlMnOpQrStUv","events":[],"pad":"{}"}}"#,
        "x".repeat(17 * 1024)
    );
    let (status, _) = app.send(beacon("application/json", large)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}
