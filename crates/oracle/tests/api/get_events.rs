use crate::helpers::{spawn_app, MockWeatherAccess};
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use hyper::{header, Method};
use nostr_sdk::Keys;
use oracle::{CreateEvent, EventSummary};
use serde_json::from_slice;
use std::sync::Arc;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn can_get_all_events() {
    let uri = String::from("/oracle/events");
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    let keys = Keys::generate();

    let new_event_1 = CreateEvent {
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
        total_allowed_entries: 5,
        number_of_values_per_entry: 6,
        number_of_places_win: 1,
    };
    let new_event_2 = CreateEvent {
        id: Uuid::now_v7(),
        start_observation_date: OffsetDateTime::now_utc(),
        end_observation_date: OffsetDateTime::now_utc(),
        signing_date: OffsetDateTime::now_utc(),
        locations: vec![
            String::from("KITH"),
            String::from("KMCD"),
            String::from("PAPG"),
            String::from("KJAN"),
        ],
        total_allowed_entries: 5,
        number_of_values_per_entry: 6,
        number_of_places_win: 1,
    };
    let new_event_3 = CreateEvent {
        id: Uuid::now_v7(),
        start_observation_date: OffsetDateTime::now_utc(),
        end_observation_date: OffsetDateTime::now_utc(),
        signing_date: OffsetDateTime::now_utc(),
        locations: vec![
            String::from("KCQW"),
            String::from("KCSM"),
            String::from("KCRW"),
            String::from("KDED"),
        ],
        total_allowed_entries: 5,
        number_of_values_per_entry: 6,
        number_of_places_win: 1,
    };
    let expected = [
        new_event_1.clone(),
        new_event_2.clone(),
        new_event_3.clone(),
    ];
    test_app
        .oracle
        .create_event(keys.public_key, new_event_1)
        .await
        .unwrap();
    test_app
        .oracle
        .create_event(keys.public_key, new_event_2)
        .await
        .unwrap();
    test_app
        .oracle
        .create_event(keys.public_key, new_event_3)
        .await
        .unwrap();

    let request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();

    let response = test_app
        .app
        .oneshot(request)
        .await
        .expect("Failed to execute request.");
    assert!(response.status().is_success());
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let res: Vec<EventSummary> = from_slice(&body).unwrap();
    for (index, event_summary) in res.iter().enumerate() {
        let cur_expect = expected.get(index).unwrap();
        // SQLite stores timestamps as seconds, so we truncate to second precision
        assert_eq!(
            event_summary.signing_date,
            cur_expect.signing_date.replace_nanosecond(0).unwrap()
        );
        assert_eq!(
            event_summary.start_observation_date,
            cur_expect
                .start_observation_date
                .replace_nanosecond(0)
                .unwrap()
        );
        assert_eq!(
            event_summary.end_observation_date,
            cur_expect
                .end_observation_date
                .replace_nanosecond(0)
                .unwrap()
        );
        assert_eq!(
            event_summary.total_allowed_entries,
            cur_expect.total_allowed_entries as i64
        );
        assert_eq!(event_summary.total_entries, 0);
        assert!(event_summary.weather.is_empty());
        assert!(event_summary.attestation.is_none());
    }
}
