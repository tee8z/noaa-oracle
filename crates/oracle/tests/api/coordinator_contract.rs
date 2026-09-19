//! The coordinator (github.com/5day4cast/coordinator) talks to these
//! endpoints with its own request and response types. These tests post the
//! exact JSON shapes that client sends and decode responses into the shapes
//! it expects, so a change here fails before a deployment does.

use crate::helpers::{MockWeatherAccess, spawn_app};
use axum::http::StatusCode;
use dlctix::{
    EventLockingConditions,
    secp::{MaybeScalar, Point},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// The coordinator's `infra::oracle::Event`, with the public nonce point.
#[derive(Debug, Deserialize)]
struct CoordinatorEvent {
    id: Uuid,
    nonce_point: Point,
    event_announcement: EventLockingConditions,
    attestation: Option<MaybeScalar>,
}

/// The coordinator's `infra::oracle::WeatherChoices`: only three of the
/// oracle's choice fields exist on that side.
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

/// The coordinator's `CreateEvent` carries competition fields the oracle
/// does not know about and no source or scoring fields.
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
    let event_id = Uuid::now_v7();

    let body = coordinator_create_event(event_id).to_string().into_bytes();
    let (status, created) = test_app
        .send(crate::helpers::signed(
            axum::http::Method::POST,
            "/oracle/events",
            body,
            &test_app.coordinator,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&created)
    );
    let raw: Value = serde_json::from_slice(&created).unwrap();
    assert!(
        raw.get("nonce").is_none(),
        "the secret nonce is never served"
    );
    assert_eq!(raw["source"], "noaa_weather");
    assert_eq!(
        raw["scoring_fields"],
        json!(["temp_high", "temp_low", "wind_speed"])
    );
    let created: CoordinatorEvent = serde_json::from_value(raw).unwrap();
    assert_eq!(created.id, event_id);
    assert!(created.attestation.is_none());
    // 2 entries, 1 place: two winner outcomes plus the refund outcome
    assert_eq!(created.event_announcement.locking_points.len(), 3);
    assert_eq!(
        created.event_announcement.locking_points[0],
        dlctix::attestation_locking_point(
            test_app.oracle.public_key(),
            created.nonce_point,
            oracle::scoring::outcome_message(&[0]),
        ),
        "locking points are recomputable from the public key and nonce point"
    );

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
    });
    let (status, body) = test_app.submit_entries(event_id, entries).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let (status, body) = test_app.get(&format!("/oracle/events/{event_id}")).await;
    assert_eq!(status, StatusCode::OK);
    let fetched: CoordinatorEvent = serde_json::from_slice(&body).unwrap();
    assert_eq!(fetched.nonce_point, created.nonce_point);
    assert_eq!(fetched.event_announcement, created.event_announcement);
}

#[tokio::test]
async fn coordinator_error_mapping_is_stable() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;

    // Unknown event: the client maps 404 to Error::NotFound.
    let (status, _) = test_app
        .get(&format!("/oracle/events/{}", Uuid::now_v7()))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Invalid event: the client maps 400 to Error::BadRequest with the body.
    let mut invalid = coordinator_create_event(Uuid::now_v7());
    invalid["number_of_places_win"] = json!(9);
    let (status, body) = test_app
        .send(crate::helpers::signed(
            axum::http::Method::POST,
            "/oracle/events",
            invalid.to_string().into_bytes(),
            &test_app.coordinator,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert!(body["error"].as_str().unwrap().contains("places"));

    // Unsigned requests never reach the oracle logic.
    let (status, _) = test_app
        .send(
            axum::http::Request::post("/oracle/events")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    coordinator_create_event(Uuid::now_v7()).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A stopped writer answers 503, which the client treats as transient.
    test_app.state.database.stop_readiness();
    let (status, _) = test_app.get("/health").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
