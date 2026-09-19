//! Every write is a NIP-98 request from an allowlisted signer. Each
//! rejection below must happen before the oracle acts on the request.

use crate::helpers::{
    MockWeatherAccess, ORIGIN, TestApp, auth_event, event_at, payload_hash, signed, spawn_app,
    with_auth,
};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use nostr::key::Keys;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};

async fn app() -> TestApp {
    spawn_app(Arc::new(MockWeatherAccess::new())).await
}

fn create_body(test_app: &TestApp) -> Vec<u8> {
    serde_json::to_vec(&event_at(test_app.clock.now())).unwrap()
}

/// A create-event request with a custom signed URL, time, or body.
fn create_request(
    keys: &Keys,
    signed_url: &str,
    signed_body: &[u8],
    sent_body: Vec<u8>,
    created_at: Option<OffsetDateTime>,
) -> Request<Body> {
    let event = auth_event(
        "POST",
        signed_url,
        Some(payload_hash(signed_body)),
        keys,
        created_at,
    );
    with_auth(Request::post("/oracle/events"), &event)
        .body(Body::from(sent_body))
        .unwrap()
}

#[tokio::test]
async fn allowlisted_coordinator_can_create_events() {
    let test_app = app().await;
    let (status, body) = test_app.create_event(&event_at(test_app.clock.now())).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn missing_or_malformed_authorization_is_rejected() {
    let test_app = app().await;
    let body = create_body(&test_app);
    let (status, _) = test_app
        .send(
            Request::post("/oracle/events")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = test_app
        .send(
            Request::post("/oracle/events")
                .header("authorization", "Nostr not-base64!")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn signatures_over_another_url_or_host_are_rejected() {
    let test_app = app().await;
    let body = create_body(&test_app);
    for url in [
        "http://attacker.test/oracle/events".to_string(),
        format!("{ORIGIN}/oracle/events/other"),
    ] {
        let request = create_request(&test_app.coordinator, &url, &body, body.clone(), None);
        let (status, _) = test_app.send(request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{url}");
    }
    // The Host header is not trusted: a matching Host does not help.
    let event = auth_event(
        "POST",
        "http://attacker.test/oracle/events",
        Some(payload_hash(&body)),
        &test_app.coordinator,
        None,
    );
    let request = with_auth(
        Request::post("/oracle/events").header("host", "attacker.test"),
        &event,
    )
    .body(Body::from(body))
    .unwrap();
    assert_eq!(test_app.send(request).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_method_and_stale_events_are_rejected() {
    let test_app = app().await;
    let body = create_body(&test_app);
    let event = auth_event(
        "GET",
        &format!("{ORIGIN}/oracle/events"),
        Some(payload_hash(&body)),
        &test_app.coordinator,
        None,
    );
    let request = with_auth(Request::post("/oracle/events"), &event)
        .body(Body::from(body.clone()))
        .unwrap();
    assert_eq!(test_app.send(request).await.0, StatusCode::UNAUTHORIZED);

    let stale = create_request(
        &test_app.coordinator,
        &format!("{ORIGIN}/oracle/events"),
        &body,
        body.clone(),
        Some(OffsetDateTime::now_utc() - Duration::minutes(5)),
    );
    assert_eq!(test_app.send(stale).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_body_changed_after_signing_is_rejected() {
    let test_app = app().await;
    let body = create_body(&test_app);
    let mut tampered = event_at(test_app.clock.now());
    tampered.total_allowed_entries = 25;
    let request = create_request(
        &test_app.coordinator,
        &format!("{ORIGIN}/oracle/events"),
        &body,
        serde_json::to_vec(&tampered).unwrap(),
        None,
    );
    assert_eq!(test_app.send(request).await.0, StatusCode::BAD_REQUEST);

    // A body without any payload tag is rejected too.
    let event = auth_event(
        "POST",
        &format!("{ORIGIN}/oracle/events"),
        None,
        &test_app.coordinator,
        None,
    );
    let request = with_auth(Request::post("/oracle/events"), &event)
        .body(Body::from(body))
        .unwrap();
    assert_eq!(test_app.send(request).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_signed_request_is_accepted_once() {
    let test_app = app().await;
    let body = create_body(&test_app);
    let event = auth_event(
        "POST",
        &format!("{ORIGIN}/oracle/events"),
        Some(payload_hash(&body)),
        &test_app.coordinator,
        None,
    );
    let request = || {
        with_auth(Request::post("/oracle/events"), &event)
            .body(Body::from(body.clone()))
            .unwrap()
    };
    assert_eq!(test_app.send(request()).await.0, StatusCode::OK);
    assert_eq!(test_app.send(request()).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn signers_need_the_right_role() {
    let test_app = app().await;
    let body = create_body(&test_app);
    for keys in [Keys::generate(), test_app.uploader.clone()] {
        let request = signed(Method::POST, "/oracle/events", body.clone(), &keys);
        assert_eq!(test_app.send(request).await.0, StatusCode::FORBIDDEN);
    }
    let request = signed(
        Method::POST,
        "/oracle/update",
        vec![],
        &test_app.coordinator,
    );
    assert_eq!(test_app.send(request).await.0, StatusCode::FORBIDDEN);
    let request = signed(Method::POST, "/oracle/update", vec![], &test_app.uploader);
    assert_eq!(test_app.send(request).await.0, StatusCode::ACCEPTED);
    test_app.state.wait_for_etl().await;
}
