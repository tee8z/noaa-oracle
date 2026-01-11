use crate::helpers::{spawn_app, MockWeatherAccess};
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use dlctix::{attestation_locking_point, attestation_secret, Outcome};
use hyper::{header, Method};
use nostr_sdk::Keys;
use oracle::{
    oracle::get_winning_bytes, AddEventEntries, AddEventEntry, CreateEvent, Event, EventStatus,
    Forecast, Observation, TemperatureUnit, ValueOptions, WeatherChoices,
};
use serde_json::from_slice;
use std::{cmp, sync::Arc};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::time::sleep;
use tower::ServiceExt;
use uuid::{ClockSequence, Timestamp, Uuid};

fn get_uuid_from_timestamp(timestamp_str: &str) -> Uuid {
    struct Context;
    impl ClockSequence for Context {
        type Output = u16;
        fn generate_sequence(&self, _ts_secs: u64, _ts_nanos: u32) -> u16 {
            0
        }
    }

    let dt = OffsetDateTime::parse(
        timestamp_str,
        &time::format_description::well_known::Rfc3339,
    )
    .expect("Valid RFC3339 timestamp");
    let ts = Timestamp::from_unix(Context, dt.unix_timestamp() as u64, dt.nanosecond());
    Uuid::new_v7(ts)
}

/// Verifies that the attestation can be used to unlock the correct DLC outcome
#[tokio::test]
async fn attestation_unlocks_correct_dlc_outcome() {
    let keys = Keys::generate();
    let mut weather_data = MockWeatherAccess::new();

    weather_data
        .expect_forecasts_data()
        .times(2)
        .returning(|_, _| Ok(mock_forecast_data()));
    weather_data
        .expect_observation_data()
        .times(2)
        .returning(|_, _| Ok(mock_observation_data()));

    let test_app = spawn_app(Arc::new(weather_data)).await;

    let start_observation_date =
        OffsetDateTime::parse("2024-08-12T00:00:00+00:00", &Rfc3339).unwrap();
    let end_observation_date =
        OffsetDateTime::parse("2024-08-13T00:00:00+00:00", &Rfc3339).unwrap();
    let signing_date = OffsetDateTime::parse("2024-08-13T03:00:00+00:00", &Rfc3339).unwrap();

    let new_event = CreateEvent {
        id: Uuid::now_v7(),
        start_observation_date,
        end_observation_date,
        signing_date,
        locations: vec![String::from("PFNO"), String::from("KSAW")],
        total_allowed_entries: 3,
        number_of_values_per_entry: 4,
        number_of_places_win: 2,
    };

    let event = test_app
        .oracle
        .create_event(keys.public_key, new_event)
        .await
        .unwrap();

    // Create entries with different predictions
    let entry_1 = AddEventEntry {
        id: get_uuid_from_timestamp("2024-08-11T00:00:00.10Z"),
        event_id: event.id,
        expected_observations: vec![
            WeatherChoices {
                stations: String::from("PFNO"),
                temp_low: Some(ValueOptions::Under),
                temp_high: None,
                wind_speed: Some(ValueOptions::Over),
            },
            WeatherChoices {
                stations: String::from("KSAW"),
                temp_low: None,
                temp_high: None,
                wind_speed: Some(ValueOptions::Over),
            },
        ],
    };
    let entry_2 = AddEventEntry {
        id: get_uuid_from_timestamp("2024-08-11T00:00:00.20Z"),
        event_id: event.id,
        expected_observations: vec![
            WeatherChoices {
                stations: String::from("PFNO"),
                temp_low: Some(ValueOptions::Par),
                temp_high: None,
                wind_speed: Some(ValueOptions::Par),
            },
            WeatherChoices {
                stations: String::from("KSAW"),
                temp_low: Some(ValueOptions::Par),
                temp_high: None,
                wind_speed: Some(ValueOptions::Over),
            },
        ],
    };
    let entry_3 = AddEventEntry {
        id: get_uuid_from_timestamp("2024-08-11T00:00:00.30Z"),
        event_id: event.id,
        expected_observations: vec![
            WeatherChoices {
                stations: String::from("PFNO"),
                temp_low: Some(ValueOptions::Over),
                temp_high: None,
                wind_speed: Some(ValueOptions::Under),
            },
            WeatherChoices {
                stations: String::from("KSAW"),
                temp_low: Some(ValueOptions::Over),
                temp_high: None,
                wind_speed: Some(ValueOptions::Under),
            },
        ],
    };

    let event_entries = AddEventEntries {
        event_id: event.id,
        entries: vec![entry_1.clone(), entry_2.clone(), entry_3.clone()],
    };
    test_app
        .oracle
        .add_event_entries(keys.public_key, event.id, event_entries.entries)
        .await
        .unwrap();

    // Run ETL to sign the event
    let request = Request::builder()
        .method(Method::POST)
        .uri(String::from("/oracle/update"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();

    let response = test_app
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("Failed to execute request.");
    assert!(response.status().is_success());
    sleep(std::time::Duration::from_secs(1)).await;

    // Get the signed event
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/oracle/events/{}", event.id))
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
    let signed_event: Event = from_slice(&body).unwrap();

    assert_eq!(signed_event.status, EventStatus::Signed);
    let attestation = signed_event.attestation.expect("Should have attestation");

    // Verify the attestation matches one of the valid outcomes in the event announcement
    let mut entries_by_score = signed_event.entries.clone();
    entries_by_score.sort_by_key(|e| cmp::Reverse(e.score));

    let mut entries_by_id = signed_event.entries.clone();
    entries_by_id.sort_by_key(|e| e.id);

    // Get winner indices (position in id-sorted list)
    let winners: Vec<usize> = entries_by_score
        .iter()
        .take(2) // number_of_places_win
        .map(|winner| {
            entries_by_id
                .iter()
                .position(|e| e.id == winner.id)
                .unwrap()
        })
        .collect();

    let winning_bytes = get_winning_bytes(winners);

    // Verify the attestation was computed correctly
    let expected_attestation = attestation_secret(
        test_app.oracle.raw_private_key(),
        signed_event.nonce,
        &winning_bytes,
    );
    assert_eq!(attestation, expected_attestation);

    // Verify the locking point matches
    let nonce_point = signed_event.nonce.base_point_mul();
    let locking_point = attestation_locking_point(
        test_app.oracle.raw_public_key(),
        nonce_point,
        &winning_bytes,
    );

    // The attestation should unlock this specific locking point
    assert!(signed_event
        .event_announcement
        .locking_points
        .contains(&locking_point));
}

/// Verifies that events before signing date are not signed
#[tokio::test]
async fn event_not_signed_before_signing_date() {
    let keys = Keys::generate();
    let mut weather_data = MockWeatherAccess::new();

    weather_data
        .expect_forecasts_data()
        .returning(|_, _| Ok(mock_forecast_data()));
    weather_data
        .expect_observation_data()
        .returning(|_, _| Ok(mock_observation_data()));

    let test_app = spawn_app(Arc::new(weather_data)).await;

    // Set signing date in the future
    let now = OffsetDateTime::now_utc();
    let start_observation_date = now - time::Duration::days(2);
    let end_observation_date = now - time::Duration::days(1);
    let signing_date = now + time::Duration::days(1); // Future!

    let new_event = CreateEvent {
        id: Uuid::now_v7(),
        start_observation_date,
        end_observation_date,
        signing_date,
        locations: vec![String::from("PFNO")],
        total_allowed_entries: 2,
        number_of_values_per_entry: 2,
        number_of_places_win: 1,
    };

    let event = test_app
        .oracle
        .create_event(keys.public_key, new_event)
        .await
        .unwrap();

    let entry = AddEventEntry {
        id: Uuid::now_v7(),
        event_id: event.id,
        expected_observations: vec![WeatherChoices {
            stations: String::from("PFNO"),
            temp_low: Some(ValueOptions::Par),
            temp_high: None,
            wind_speed: None,
        }],
    };

    test_app
        .oracle
        .add_event_entries(keys.public_key, event.id, vec![entry])
        .await
        .unwrap();

    // Run ETL
    let request = Request::builder()
        .method(Method::POST)
        .uri(String::from("/oracle/update"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();

    test_app
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("Failed to execute request.");
    sleep(std::time::Duration::from_secs(1)).await;

    // Get event - should NOT be signed yet
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/oracle/events/{}", event.id))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();

    let response = test_app
        .app
        .oneshot(request)
        .await
        .expect("Failed to execute request.");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let result: Event = from_slice(&body).unwrap();

    // Event should be completed but NOT signed (signing date hasn't passed)
    assert_ne!(result.status, EventStatus::Signed);
    assert!(result.attestation.is_none());
}

/// Verifies that the nonce is unique per event
#[tokio::test]
async fn each_event_has_unique_nonce() {
    let keys = Keys::generate();
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;

    let now = OffsetDateTime::now_utc();

    let event1 = CreateEvent {
        id: Uuid::now_v7(),
        start_observation_date: now,
        end_observation_date: now,
        signing_date: now,
        locations: vec![String::from("PFNO")],
        total_allowed_entries: 2,
        number_of_values_per_entry: 2,
        number_of_places_win: 1,
    };

    let event2 = CreateEvent {
        id: Uuid::now_v7(),
        start_observation_date: now,
        end_observation_date: now,
        signing_date: now,
        locations: vec![String::from("PFNO")],
        total_allowed_entries: 2,
        number_of_values_per_entry: 2,
        number_of_places_win: 1,
    };

    let created1 = test_app
        .oracle
        .create_event(keys.public_key, event1)
        .await
        .unwrap();
    let created2 = test_app
        .oracle
        .create_event(keys.public_key, event2)
        .await
        .unwrap();

    // Nonces must be different for security
    assert_ne!(
        created1.nonce, created2.nonce,
        "Each event must have a unique nonce"
    );
}

/// Verifies that the event announcement contains the correct number of outcomes
#[tokio::test]
async fn event_announcement_has_correct_outcome_count() {
    let keys = Keys::generate();
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;

    let now = OffsetDateTime::now_utc();

    // 5 entries, 3 places = P(5,3) + 1 = 60 + 1 = 61 outcomes
    let event = CreateEvent {
        id: Uuid::now_v7(),
        start_observation_date: now,
        end_observation_date: now,
        signing_date: now,
        locations: vec![String::from("PFNO")],
        total_allowed_entries: 5,
        number_of_values_per_entry: 2,
        number_of_places_win: 3,
    };

    let created = test_app
        .oracle
        .create_event(keys.public_key, event)
        .await
        .unwrap();

    // Locking points = permutations + 1 (for refund)
    // P(5,3) = 5!/(5-3)! = 5*4*3 = 60
    // Total = 60 + 1 = 61
    let expected_outcomes = 61;
    assert_eq!(
        created.event_announcement.locking_points.len(),
        expected_outcomes,
        "Should have {} locking points (P(5,3) + 1 refund)",
        expected_outcomes
    );

    // Verify attestation outcomes are valid
    assert!(created
        .event_announcement
        .is_valid_outcome(&Outcome::Attestation(0)));
    assert!(created
        .event_announcement
        .is_valid_outcome(&Outcome::Attestation(1)));
}

/// Verifies attestation is deterministic for same inputs
#[tokio::test]
async fn attestation_is_deterministic() {
    let keys = Keys::generate();
    let mut weather_data = MockWeatherAccess::new();

    weather_data
        .expect_forecasts_data()
        .returning(|_, _| Ok(mock_forecast_data()));
    weather_data
        .expect_observation_data()
        .returning(|_, _| Ok(mock_observation_data()));

    let test_app = spawn_app(Arc::new(weather_data)).await;

    let start_observation_date =
        OffsetDateTime::parse("2024-08-12T00:00:00+00:00", &Rfc3339).unwrap();
    let end_observation_date =
        OffsetDateTime::parse("2024-08-13T00:00:00+00:00", &Rfc3339).unwrap();
    let signing_date = OffsetDateTime::parse("2024-08-13T03:00:00+00:00", &Rfc3339).unwrap();

    let event = CreateEvent {
        id: Uuid::now_v7(),
        start_observation_date,
        end_observation_date,
        signing_date,
        locations: vec![String::from("PFNO")],
        total_allowed_entries: 2,
        number_of_values_per_entry: 2,
        number_of_places_win: 1,
    };

    let created = test_app
        .oracle
        .create_event(keys.public_key, event)
        .await
        .unwrap();

    let entry = AddEventEntry {
        id: get_uuid_from_timestamp("2024-08-11T00:00:00.10Z"),
        event_id: created.id,
        expected_observations: vec![WeatherChoices {
            stations: String::from("PFNO"),
            temp_low: Some(ValueOptions::Under),
            temp_high: None,
            wind_speed: Some(ValueOptions::Over),
        }],
    };

    test_app
        .oracle
        .add_event_entries(keys.public_key, created.id, vec![entry.clone()])
        .await
        .unwrap();

    // Get the event before ETL
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/oracle/events/{}", created.id))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap();

    let response = test_app.app.clone().oneshot(request).await.unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let before_etl: Event = from_slice(&body).unwrap();

    // Manually compute what the attestation should be
    let winners = vec![0usize]; // Only one entry, it wins
    let winning_bytes = get_winning_bytes(winners);
    let expected_attestation = attestation_secret(
        test_app.oracle.raw_private_key(),
        before_etl.nonce,
        &winning_bytes,
    );

    // Run ETL
    let request = Request::builder()
        .method(Method::POST)
        .uri(String::from("/oracle/update"))
        .body(Body::empty())
        .unwrap();
    test_app.app.clone().oneshot(request).await.unwrap();
    sleep(std::time::Duration::from_secs(1)).await;

    // Get the signed event
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/oracle/events/{}", created.id))
        .body(Body::empty())
        .unwrap();
    let response = test_app.app.oneshot(request).await.unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let after_etl: Event = from_slice(&body).unwrap();

    assert_eq!(after_etl.status, EventStatus::Signed);
    assert_eq!(
        after_etl.attestation.unwrap(),
        expected_attestation,
        "Attestation should be deterministic"
    );
}

fn mock_forecast_data() -> Vec<Forecast> {
    vec![
        Forecast {
            station_id: String::from("PFNO"),
            date: String::from("2024-08-12"),
            start_time: String::from("2024-08-11T00:00:00+00:00"),
            end_time: String::from("2024-08-12T00:00:00+00:00"),
            temp_low: 9,
            temp_high: 35,
            wind_speed: Some(8),
            temp_unit_code: TemperatureUnit::Fahrenheit.to_string(),
        },
        Forecast {
            station_id: String::from("KSAW"),
            date: String::from("2024-08-12"),
            start_time: String::from("2024-08-11T00:00:00+00:00"),
            end_time: String::from("2024-08-12T00:00:00+00:00"),
            temp_low: 17,
            temp_high: 25,
            wind_speed: Some(3),
            temp_unit_code: TemperatureUnit::Fahrenheit.to_string(),
        },
    ]
}

fn mock_observation_data() -> Vec<Observation> {
    vec![
        Observation {
            station_id: String::from("PFNO"),
            start_time: String::from("2024-08-12T00:00:00+00:00"),
            end_time: String::from("2024-08-13T00:00:00+00:00"),
            temp_low: 9.4,
            temp_high: 35.0,
            wind_speed: 11,
            temp_unit_code: TemperatureUnit::Fahrenheit.to_string(),
        },
        Observation {
            station_id: String::from("KSAW"),
            start_time: String::from("2024-08-12T00:00:00+00:00"),
            end_time: String::from("2024-08-13T00:00:00+00:00"),
            temp_low: 22.0,
            temp_high: 25.0,
            wind_speed: 10,
            temp_unit_code: TemperatureUnit::Fahrenheit.to_string(),
        },
    ]
}
