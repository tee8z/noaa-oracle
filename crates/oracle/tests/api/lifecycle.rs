//! An event from creation to attestation, driven by the injected clock and
//! mocked NOAA data. The attestation must unlock exactly the locking point
//! of the winning outcome, and never change once published.

use crate::helpers::{MockWeatherAccess, TestApp, event_at, spawn_app};
use axum::http::StatusCode;
use dlctix::{attestation_locking_point, secp::MaybePoint};
use oracle::{
    Event, EventStatus, Forecast, ForecastRequest, Observation, scoring::outcome_message,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use time::{Duration, Time, UtcOffset, format_description::well_known::Rfc3339};
use uuid::{NoContext, Timestamp, Uuid};

fn forecast(station: &str, temp_high: i64, temp_low: i64, wind_speed: i64) -> Forecast {
    Forecast {
        station_id: station.into(),
        date: "2030-01-01".into(),
        start_time: String::new(),
        end_time: String::new(),
        temp_low,
        temp_high,
        wind_speed: Some(wind_speed),
        wind_direction: None,
        humidity_max: None,
        humidity_min: None,
        temp_unit_code: "fahrenheit".into(),
        precip_chance: None,
        rain_amt: None,
        snow_amt: None,
        ice_amt: None,
    }
}

fn observation(station: &str, temp_high: f64, temp_low: f64, wind_speed: i64) -> Observation {
    Observation {
        station_id: station.into(),
        start_time: "2030-01-01T00:00:00Z".into(),
        end_time: "2030-01-02T00:00:00Z".into(),
        latest_temp: None,
        latest_temp_time: None,
        temp_low,
        temp_high,
        wind_speed: Some(wind_speed),
        temp_unit_code: "fahrenheit".into(),
        wind_direction: None,
        humidity: None,
        rain_amt: None,
        snow_amt: None,
        ice_amt: None,
    }
}

fn forecasts_in_window(
    request: &ForecastRequest,
    stations: &[(&str, i64, i64, i64)],
) -> Vec<Forecast> {
    let start = request.start.unwrap().to_offset(UtcOffset::UTC);
    let end = request.end.unwrap().to_offset(UtcOffset::UTC);
    let last_day = (end - Duration::nanoseconds(1)).date();
    std::iter::successors(Some(start.date()), |date| date.next_day())
        .take_while(|date| *date <= last_day)
        .flat_map(|date| {
            stations.iter().map(move |&(station, high, low, wind)| {
                let mut forecast = forecast(station, high, low, wind);
                forecast.date = date.to_string();
                forecast.start_time = date
                    .with_time(Time::MIDNIGHT)
                    .assume_utc()
                    .max(start)
                    .format(&Rfc3339)
                    .unwrap();
                forecast.end_time = (date.with_time(Time::MIDNIGHT).assume_utc()
                    + Duration::days(1))
                .min(end)
                .format(&Rfc3339)
                .unwrap();
                forecast
            })
        })
        .collect()
}

/// KORD: forecast 70/50/10, observed 75/50/10. KSAW: forecast 40/30/5,
/// observed 40/29/8.
async fn app_with_weather() -> TestApp {
    let mut weather = MockWeatherAccess::new();
    weather.expect_forecasts_data().returning(|request, _| {
        Ok(forecasts_in_window(
            request,
            &[("KORD", 70, 50, 10), ("KSAW", 40, 30, 5)],
        ))
    });
    weather.expect_observation_data().returning(|_, _| {
        Ok(vec![
            observation("KORD", 75.0, 50.0, 10),
            observation("KSAW", 40.0, 29.4, 8),
        ])
    });
    spawn_app(Arc::new(weather)).await
}

fn pick(target: &str, metric: &str, prediction: &str) -> Value {
    json!({"target": target, "metric": metric, "prediction": prediction})
}

/// Creates an event and submits entries with the given picks, in id order.
async fn event_with_entries(test_app: &TestApp, picks: Vec<Vec<Value>>) -> (Event, Vec<Uuid>) {
    let ids = picks.iter().map(|_| Uuid::now_v7()).collect();
    event_with_entry_ids(test_app, picks, ids).await
}

async fn event_with_entry_ids(
    test_app: &TestApp,
    picks: Vec<Vec<Value>>,
    ids: Vec<Uuid>,
) -> (Event, Vec<Uuid>) {
    let (status, body) = test_app.create_event(&event_at(test_app.clock.now())).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let event: Event = serde_json::from_slice(&body).unwrap();
    let entries: Vec<Value> = ids
        .iter()
        .zip(picks)
        .map(|(id, picks)| json!({"id": id, "event_id": event.id, "picks": picks}))
        .collect();
    let (status, body) = test_app
        .submit_entries(event.id, json!({"event_id": event.id, "entries": entries}))
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    (event, ids)
}

async fn fetch(test_app: &TestApp, id: Uuid) -> Event {
    test_app.get_json(&format!("/oracle/events/{id}")).await
}

fn assert_attests(test_app: &TestApp, event: &Event, winners: &[usize]) {
    let attestation = event.attestation.expect("attested");
    let locking_point = attestation_locking_point(
        test_app.oracle.public_key(),
        event.nonce_point,
        outcome_message(winners),
    );
    assert!(matches!(locking_point, MaybePoint::Valid(_)));
    assert_eq!(attestation.base_point_mul(), locking_point);
    assert!(
        event
            .event_announcement
            .locking_points
            .contains(&locking_point)
    );
}

#[tokio::test]
async fn the_best_entry_is_attested_after_the_signing_date() {
    let test_app = app_with_weather().await;
    let (event, ids) = event_with_entries(
        &test_app,
        vec![
            // 10 (over) + 20 (par) = 30
            vec![
                pick("KORD", "temp_high", "Over"),
                pick("KORD", "temp_low", "Par"),
            ],
            // 0 + 10 (29.4 rounds under 30) = 10
            vec![
                pick("KORD", "temp_high", "Under"),
                pick("KSAW", "temp_low", "Under"),
            ],
            // 20 + 20 = 40: the winner
            vec![
                pick("KSAW", "temp_high", "Par"),
                pick("KORD", "wind_speed", "Par"),
            ],
        ],
    )
    .await;

    // Before the window nothing is scored, though forecasts are recorded.
    test_app.run_etl().await;
    let live = fetch(&test_app, event.id).await;
    assert_eq!(live.status, EventStatus::Live);
    assert!(live.entries.iter().all(|entry| entry.score.is_none()));
    assert_eq!(live.weather.len(), 2);
    assert_eq!(live.weather[0].forecasted.temp_high, 70);

    // Inside the window entries are scored but nothing is signed.
    test_app
        .clock
        .set(event.start_observation_date + Duration::hours(1));
    test_app.run_etl().await;
    let running = fetch(&test_app, event.id).await;
    assert_eq!(running.status, EventStatus::Running);
    let base_scores: Vec<Option<i64>> = running.entries.iter().map(|e| e.base_score).collect();
    assert_eq!(base_scores, vec![Some(30), Some(10), Some(40)]);
    assert!(running.attestation.is_none());

    // Completed but before the signing date: still unsigned.
    test_app.clock.set(event.end_observation_date);
    test_app.run_etl().await;
    assert!(fetch(&test_app, event.id).await.attestation.is_none());

    test_app.clock.set(event.signing_date);
    test_app.run_etl().await;
    let signed = fetch(&test_app, event.id).await;
    assert_eq!(signed.status, EventStatus::Signed);
    assert_eq!(signed.entry_ids, ids);
    assert_attests(&test_app, &signed, &[2]);

    // Later passes never change a published attestation.
    test_app.clock.set(event.signing_date + Duration::hours(1));
    test_app.run_etl().await;
    assert_eq!(
        fetch(&test_app, event.id).await.attestation,
        signed.attestation
    );
}

#[tokio::test]
async fn when_nobody_scores_the_refund_outcome_is_attested() {
    let test_app = app_with_weather().await;
    let (event, _) = event_with_entries(
        &test_app,
        vec![
            vec![pick("KORD", "temp_high", "Under")],
            vec![pick("KORD", "temp_low", "Over")],
            vec![pick("KSAW", "temp_high", "Over")],
        ],
    )
    .await;
    test_app.clock.set(event.signing_date);
    test_app.run_etl().await;
    let signed = fetch(&test_app, event.id).await;
    assert_attests(&test_app, &signed, &[0, 1, 2]);
    let refund = signed.event_announcement.locking_points.last().unwrap();
    assert_eq!(signed.attestation.unwrap().base_point_mul(), *refund);
}

#[tokio::test]
async fn events_without_entries_are_never_signed() {
    let test_app = app_with_weather().await;
    let (status, body) = test_app.create_event(&event_at(test_app.clock.now())).await;
    assert_eq!(status, StatusCode::OK);
    let event: Event = serde_json::from_slice(&body).unwrap();
    test_app.clock.set(event.signing_date);
    test_app.run_etl().await;
    let unsigned = fetch(&test_app, event.id).await;
    assert!(unsigned.attestation.is_none());
    assert_eq!(unsigned.status, EventStatus::Completed);
}

#[tokio::test]
async fn one_failing_event_does_not_block_the_others() {
    let mut weather = MockWeatherAccess::new();
    weather.expect_forecasts_data().returning(|request, _| {
        if request.station_ids.contains("KMSP") {
            Err(oracle::weather_data::Error::InvalidStationId("KMSP".into()))
        } else {
            Ok(forecasts_in_window(
                request,
                &[("KORD", 70, 50, 10), ("KSAW", 40, 30, 5)],
            ))
        }
    });
    weather
        .expect_observation_data()
        .returning(|_, _| Ok(vec![observation("KORD", 75.0, 50.0, 10)]));
    let test_app = spawn_app(Arc::new(weather)).await;
    let mut failing = event_at(test_app.clock.now());
    failing.locations = vec!["KMSP".into()];
    failing.number_of_values_per_entry = 1;
    assert_eq!(test_app.create_event(&failing).await.0, StatusCode::OK);
    let (event, _) = event_with_entries(
        &test_app,
        vec![
            vec![pick("KORD", "temp_high", "Over")],
            vec![pick("KORD", "temp_high", "Under")],
            vec![pick("KORD", "temp_high", "Par")],
        ],
    )
    .await;
    test_app.clock.set(event.signing_date);
    let failures = test_app.oracle.etl_data(1).await.unwrap();
    assert_eq!(failures, 1);
    assert_attests(&test_app, &fetch(&test_app, event.id).await, &[0]);
}

#[tokio::test]
async fn forecast_revisions_during_the_event_do_not_change_the_baseline_or_scores() {
    let revised = Arc::new(AtomicBool::new(false));
    let mut weather = MockWeatherAccess::new();
    let forecast_revised = revised.clone();
    weather
        .expect_forecasts_data()
        .returning(move |request, _| {
            let start = request.start.unwrap();
            assert_eq!(request.generated_start, Some(start - Duration::days(7)));
            assert_eq!(
                request.generated_end,
                Some(start - Duration::nanoseconds(1))
            );
            // A query without the pre-event cutoff would now see the revised
            // 75°F forecast and change the winner from Over to Par.
            let high = if forecast_revised.load(Ordering::SeqCst)
                && request.generated_end.is_none_or(|end| end >= start)
            {
                75
            } else {
                70
            };
            Ok(forecasts_in_window(request, &[("KORD", high, 50, 10)]))
        });
    weather.expect_observation_data().returning(|request, _| {
        let start = request.start.unwrap();
        assert_eq!(
            request.end,
            Some(start + Duration::hours(24) - Duration::nanoseconds(1))
        );
        Ok(vec![observation("KORD", 75.0, 50.0, 10)])
    });
    let app = spawn_app(Arc::new(weather)).await;
    let (event, _) = event_with_entries(
        &app,
        vec![
            vec![pick("KORD", "temp_high", "Over")],
            vec![pick("KORD", "temp_high", "Par")],
            vec![pick("KORD", "temp_high", "Under")],
        ],
    )
    .await;
    app.clock
        .set(event.start_observation_date + Duration::hours(1));
    app.run_etl().await;
    let before = fetch(&app, event.id).await;
    let scores: Vec<_> = before
        .entries
        .iter()
        .map(|entry| entry.base_score)
        .collect();
    assert_eq!(scores, vec![Some(10), Some(0), Some(0)]);
    assert_eq!(before.weather[0].forecasted.temp_high, 70);

    revised.store(true, Ordering::SeqCst);
    app.clock.set(event.signing_date);
    app.run_etl().await;
    let after = fetch(&app, event.id).await;
    assert_eq!(
        after
            .entries
            .iter()
            .map(|entry| entry.base_score)
            .collect::<Vec<_>>(),
        scores
    );
    assert_eq!(after.weather[0].forecasted.temp_high, 70);
    assert_attests(&app, &after, &[0]);
}

#[tokio::test]
async fn equal_scores_across_a_ten_second_boundary_attest_the_earlier_entry() {
    let app = app_with_weather().await;
    let boundary = app.clock.now().unix_timestamp() as u64 / 10 * 10_000;
    let entry_id = |millis: u64| {
        Uuid::new_v7(Timestamp::from_unix(
            NoContext,
            millis / 1000,
            (millis % 1000) as u32 * 1_000_000,
        ))
    };
    let ids = vec![
        entry_id(boundary - 1),
        entry_id(boundary),
        entry_id(boundary + 1),
    ];
    let (event, ids) = event_with_entry_ids(
        &app,
        vec![
            vec![pick("KORD", "temp_high", "Over")],
            vec![pick("KORD", "temp_high", "Over")],
            vec![pick("KORD", "temp_high", "Under")],
        ],
        ids,
    )
    .await;
    app.clock.set(event.signing_date);
    app.run_etl().await;
    let signed = fetch(&app, event.id).await;
    assert_eq!(signed.entry_ids, ids);
    assert_eq!(signed.entries[0].base_score, Some(10));
    assert_eq!(signed.entries[1].base_score, Some(10));
    assert_eq!(signed.entries[0].score, signed.entries[1].score);
    assert_attests(&app, &signed, &[0]);
}
