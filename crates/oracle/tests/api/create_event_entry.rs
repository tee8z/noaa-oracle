use crate::helpers::{create_auth_event, spawn_app, MockWeatherAccess};
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use hyper::{header, Method};
use log::{debug, info};
use nostr_sdk::{
    hashes::{sha256::Hash as Sha256Hash, Hash},
    Keys,
};
use oracle::{AddEventEntries, AddEventEntry, CreateEvent, WeatherChoices, WeatherEntry};
use serde_json::{from_slice, to_string};
use std::sync::Arc;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn can_create_entry_into_event() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    let keys = Keys::generate();
    let oracle_event_id = Uuid::now_v7();
    let new_event = CreateEvent {
        id: oracle_event_id,
        start_observation_date: OffsetDateTime::now_utc(),
        end_observation_date: OffsetDateTime::now_utc(),
        signing_date: OffsetDateTime::now_utc(),
        locations: vec![
            String::from("PFNO"),
            String::from("KSAW"),
            String::from("PAPG"),
            String::from("KWMC"),
        ],
        total_allowed_entries: 1,
        number_of_values_per_entry: 6,
        number_of_places_win: 1,
    };

    let new_entry = AddEventEntry {
        id: Uuid::now_v7(),
        event_id: oracle_event_id,
        expected_observations: vec![
            WeatherChoices {
                stations: String::from("PFNO"),
                temp_low: Some(oracle::ValueOptions::Par),
                temp_high: None,
                wind_speed: None,
            },
            WeatherChoices {
                stations: String::from("KSAW"),
                temp_low: Some(oracle::ValueOptions::Par),
                temp_high: None,
                wind_speed: Some(oracle::ValueOptions::Over),
            },
            WeatherChoices {
                stations: String::from("KWMC"),
                temp_low: Some(oracle::ValueOptions::Par),
                temp_high: Some(oracle::ValueOptions::Under),
                wind_speed: None,
            },
        ],
    };
    let entries = AddEventEntries {
        event_id: new_event.id,
        entries: vec![new_entry.clone()],
    };
    let body_json = to_string(&entries).unwrap();
    let payload_hash = Sha256Hash::hash(body_json.as_bytes());

    let oracle_event = test_app
        .oracle
        .create_event(keys.public_key, new_event)
        .await
        .unwrap();

    let base_url = "http://localhost:3000";
    let path = format!("/oracle/events/{}/entries", oracle_event.id);
    let event = create_auth_event(
        "POST",
        &format!("{}{}", base_url, path),
        Some(payload_hash),
        &keys,
    )
    .await;

    let auth_header = format!(
        "Nostr {}",
        BASE64.encode(serde_json::to_string(&event).unwrap())
    );
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, auth_header)
        .header("host", "localhost:3000")
        .body(Body::from(body_json))
        .unwrap();

    let response = test_app
        .app
        .oneshot(request)
        .await
        .expect("Failed to execute request.");
    info!("response status: {}", response.status());
    assert!(response.status().is_success());
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    debug!("body: {:?}", body);
    let res: Vec<WeatherEntry> = from_slice(&body).unwrap();
    assert_eq!(res[0].event_id, new_entry.event_id);
    assert_eq!(res[0].id, new_entry.id);
    assert_eq!(
        res[0].expected_observations,
        new_entry.expected_observations
    );
}

#[tokio::test]
async fn can_create_and_get_event_entry() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    let keys = Keys::generate();
    let new_event = CreateEvent {
        id: Uuid::now_v7(),
        start_observation_date: OffsetDateTime::now_utc(),
        end_observation_date: OffsetDateTime::now_utc(),
        signing_date: OffsetDateTime::now_utc(),
        locations: vec![
            String::from("PFNO"),
            String::from("KSAW"),
            String::from("PAPG"),
            String::from("KWMC"),
        ],
        total_allowed_entries: 1,
        number_of_places_win: 1,
        number_of_values_per_entry: 6,
    };
    let new_entry = AddEventEntry {
        id: Uuid::now_v7(),
        event_id: new_event.id,
        expected_observations: vec![
            WeatherChoices {
                stations: String::from("PFNO"),
                temp_low: Some(oracle::ValueOptions::Par),
                temp_high: None,
                wind_speed: None,
            },
            WeatherChoices {
                stations: String::from("KSAW"),
                temp_low: Some(oracle::ValueOptions::Par),
                temp_high: None,
                wind_speed: Some(oracle::ValueOptions::Over),
            },
            WeatherChoices {
                stations: String::from("KWMC"),
                temp_low: Some(oracle::ValueOptions::Par),
                temp_high: Some(oracle::ValueOptions::Under),
                wind_speed: None,
            },
        ],
    };
    let entries = AddEventEntries {
        event_id: new_entry.event_id,
        entries: vec![new_entry],
    };
    let body_json = to_string(&entries).unwrap();
    let payload_hash = Sha256Hash::hash(body_json.as_bytes());
    let base_url = "http://localhost:3000";
    let path = format!("/oracle/events/{}/entries", new_event.id);
    let event = create_auth_event(
        "POST",
        &format!("{}{}", base_url, path),
        Some(payload_hash),
        &keys,
    )
    .await;
    let auth_header = format!(
        "Nostr {}",
        BASE64.encode(serde_json::to_string(&event).unwrap())
    );
    let oracle_event = test_app
        .oracle
        .create_event(event.pubkey, new_event)
        .await
        .unwrap();

    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, auth_header)
        .header("host", "localhost:3000")
        .body(Body::from(body_json))
        .unwrap();

    let response = test_app
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("Failed to execute request.");
    info!("response status: {:?}", response);
    assert!(response.status().is_success());
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let res_post: Vec<WeatherEntry> = from_slice(&body).unwrap();

    let request_get = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "/oracle/events/{}/entries/{}",
            oracle_event.id, res_post[0].id
        ))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();

    let response_get = test_app
        .app
        .oneshot(request_get)
        .await
        .expect("Failed to execute request.");

    assert!(response_get.status().is_success());
    let body = to_bytes(response_get.into_body(), usize::MAX)
        .await
        .unwrap();
    let res: WeatherEntry = from_slice(&body).unwrap();
    assert_eq!(res_post[0].id, res.id);
    assert_eq!(res_post[0].event_id, res.event_id);
    assert_eq!(res_post[0].score, res.score);
    assert_eq!(res_post[0].expected_observations, res.expected_observations);
}
