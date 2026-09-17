//! The coordinator (github.com/5day4cast/coordinator) talks to these
//! endpoints with its own request and response types. These tests post the
//! exact JSON shapes that client sends and decode responses into the shapes
//! it expects, so a change here fails before a deployment does.

use crate::helpers::{MockWeatherAccess, create_auth_event, payload_hash, spawn_app};
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use dlctix::{
    EventLockingConditions,
    secp::{MaybeScalar, Scalar},
};
use nostr::key::Keys;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

/// Mirror of the coordinator's `infra::oracle::Event`.
#[derive(Debug, Deserialize)]
struct CoordinatorEvent {
    id: Uuid,
    nonce: Scalar,
    event_announcement: EventLockingConditions,
    attestation: Option<MaybeScalar>,
}

/// Mirror of the coordinator's `infra::oracle::WeatherChoices`: only three
/// of the oracle's choice fields exist on that side.
#[derive(Debug, Serialize)]
struct CoordinatorWeatherChoices {
    stations: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    wind_speed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temp_high: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temp_low: Option<String>,
}

const BASE_URL: &str = "http://localhost:3000";

fn authorized(method: Method, path: &str, body: &str, keys: &Keys) -> Request<Body> {
    let event = create_auth_event(
        method.as_str(),
        &format!("{BASE_URL}{path}"),
        Some(payload_hash(body.as_bytes())),
        keys,
    );
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::AUTHORIZATION,
            format!(
                "Nostr {}",
                BASE64.encode(serde_json::to_string(&event).unwrap())
            ),
        )
        .header("host", "localhost:3000")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

/// The coordinator's `CreateEvent` carries competition fields the oracle
/// does not know about; the oracle must ignore them.
fn coordinator_create_event(id: Uuid) -> Value {
    json!({
        "id": id,
        "signing_date": "2030-01-02T03:00:00Z",
        "start_observation_date": "2030-01-01T00:00:00Z",
        "end_observation_date": "2030-01-02T00:00:00Z",
        "locations": ["KORD", "KSAW"],
        "number_of_values_per_entry": 6,
        "number_of_places_win": 1,
        "total_allowed_entries": 2,
        "entry_fee": 1000,
        "coordinator_fee_percentage": 5,
        "total_competition_pool": 2000,
        "relative_locktime_block_delta": null
    })
}

#[tokio::test]
async fn coordinator_event_lifecycle_uses_stable_wire_shapes() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    let keys = Keys::generate();
    let event_id = Uuid::now_v7();

    let body = coordinator_create_event(event_id).to_string();
    let response = test_app
        .app
        .clone()
        .oneshot(authorized(Method::POST, "/oracle/events", &body, &keys))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let created: CoordinatorEvent =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(created.id, event_id);
    assert!(created.attestation.is_none());
    // 2 entries, 1 place: two winner outcomes plus the refund outcome
    assert_eq!(created.event_announcement.locking_points.len(), 3);

    let entries = json!({
        "event_id": event_id,
        "entries": [
            {
                "id": Uuid::now_v7(),
                "event_id": event_id,
                "expected_observations": [
                    CoordinatorWeatherChoices {
                        stations: "KORD".into(),
                        wind_speed: Some("Over".into()),
                        temp_high: Some("Par".into()),
                        temp_low: None,
                    }
                ]
            },
            {
                "id": Uuid::now_v7(),
                "event_id": event_id,
                "expected_observations": [
                    CoordinatorWeatherChoices {
                        stations: "KSAW".into(),
                        wind_speed: None,
                        temp_high: None,
                        temp_low: Some("Under".into()),
                    }
                ]
            }
        ]
    })
    .to_string();
    let path = format!("/oracle/events/{event_id}/entries");
    let response = test_app
        .app
        .clone()
        .oneshot(authorized(Method::POST, &path, &entries, &keys))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = test_app
        .app
        .clone()
        .oneshot(authorized(
            Method::GET,
            &format!("/oracle/events/{event_id}"),
            "",
            &keys,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let fetched: CoordinatorEvent =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(fetched.nonce, created.nonce);
    assert_eq!(fetched.event_announcement, created.event_announcement);
}

#[tokio::test]
async fn coordinator_error_mapping_is_stable() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    let keys = Keys::generate();

    // Unknown event: the client maps 404 to Error::NotFound.
    let missing = Uuid::now_v7();
    let response = test_app
        .app
        .clone()
        .oneshot(authorized(
            Method::GET,
            &format!("/oracle/events/{missing}"),
            "",
            &keys,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Invalid event: the client maps 400 to Error::BadRequest with the body.
    let mut invalid = coordinator_create_event(Uuid::now_v7());
    invalid["number_of_places_win"] = json!(9);
    let response = test_app
        .app
        .clone()
        .oneshot(authorized(
            Method::POST,
            "/oracle/events",
            &invalid.to_string(),
            &keys,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(body["error"].as_str().unwrap().contains("ranks"));

    // Unsigned requests never reach the oracle logic.
    let response = test_app
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/oracle/events")
                .header(header::CONTENT_TYPE, "application/json")
                .header("host", "localhost:3000")
                .body(Body::from(
                    coordinator_create_event(Uuid::now_v7()).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A stopped writer answers 503, which the client treats as transient.
    test_app.state.database.stop_readiness();
    let response = test_app
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
