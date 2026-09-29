//! An event from creation to attestation, driven by the injected clock and
//! mocked NOAA data. The attestation must unlock exactly the locking point
//! of the winning outcome, and never change once published.

use crate::helpers::{MockWeatherAccess, TestApp, event_at, fitted_line, metric, spawn_app};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use dlctix::{attestation_locking_point, secp::MaybePoint};
use oracle::{
    Event, EventStatus, Forecast, ForecastRequest, Observation, scoring::outcome_message,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use time::{Duration, Time, UtcOffset, format_description::well_known::Rfc3339};
use uuid::{NoContext, Timestamp, Uuid};

/// Explicitly configure both provisional and audited fixture paths. Production
/// readers must never implement the strict methods by delegating this way.
fn set_forecasts<F>(weather: &mut MockWeatherAccess, result: F)
where
    F: Fn(&ForecastRequest, Vec<String>) -> Result<Vec<Forecast>, oracle::weather_data::Error>
        + Clone
        + Send
        + 'static,
{
    weather.expect_forecasts_data().returning(result.clone());
    let assessed = result.clone();
    weather
        .expect_forecast_assessment()
        .returning(move |request, stations| assessed(request, stations).map(forecast_assessments));
    weather
        .expect_settlement_forecasts()
        .returning(move |request, stations| {
            result(request, stations).map(settlement_forecast_values)
        });
}

/// The whole-period baselines provisional readings use, from the same daily
/// fixture forecasts as settlement.
fn forecast_assessments(rows: Vec<Forecast>) -> Vec<oracle::weather_data::ForecastAssessment> {
    settlement_forecast_values(rows)
        .into_iter()
        .map(|value| oracle::weather_data::ForecastAssessment {
            reason: value.value.is_none().then(|| "no forecast".into()),
            station_id: value.station_id,
            metric: value.metric,
            value: value.value,
            native_intervals: vec![],
        })
        .collect()
}

/// Convert daily fixture forecasts into the strict whole-window value shape.
fn settlement_forecast_values(rows: Vec<Forecast>) -> Vec<oracle::weather_data::SettlementValue> {
    use std::collections::BTreeMap;
    let mut values: BTreeMap<(String, &str), Option<f64>> = BTreeMap::new();
    for row in rows {
        for (metric, value) in [
            ("temp_high", Some(row.temp_high as f64)),
            ("temp_low", Some(row.temp_low as f64)),
            ("wind_speed", row.wind_speed.map(|value| value as f64)),
            (
                "wind_direction",
                row.wind_direction.map(|value| value as f64),
            ),
            ("rain_amt", row.rain_amt),
            ("snow_amt", row.snow_amt),
            ("humidity", row.humidity_max.map(|value| value as f64)),
        ] {
            values
                .entry((row.station_id.clone(), metric))
                .and_modify(|existing| {
                    *existing = existing.zip(value).map(|(a, b)| match metric {
                        "rain_amt" | "snow_amt" => a + b,
                        "temp_low" => a.min(b),
                        _ => a.max(b),
                    });
                })
                .or_insert(value);
        }
    }
    values
        .into_iter()
        .map(
            |((station_id, metric), value)| oracle::weather_data::SettlementValue {
                station_id,
                metric: metric.into(),
                value,
            },
        )
        .collect()
}

fn set_observations<F>(weather: &mut MockWeatherAccess, result: F)
where
    F: Fn(
            &oracle::ObservationRequest,
            Vec<String>,
        ) -> Result<Vec<Observation>, oracle::weather_data::Error>
        + Clone
        + Send
        + 'static,
{
    weather.expect_observation_data().returning(result.clone());
    weather.expect_settlement_observations().returning(
        move |request, stations, cutoff, metrics| {
            // All lifecycle fixtures configure a two-hour signing grace.
            assert_eq!(cutoff, request.end.unwrap() + Duration::hours(2));
            assert!(!metrics.is_empty(), "settlement passes the event's metrics");
            result(request, stations)
        },
    );
}

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
    set_forecasts(&mut weather, |request, _| {
        Ok(forecasts_in_window(
            request,
            &[("KORD", 70, 50, 10), ("KSAW", 40, 30, 5)],
        ))
    });
    set_observations(&mut weather, |_, _| {
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

/// A pass told to stop leaves its events for the next one, so a shutting-down process can
/// release the processing lease at once.
#[tokio::test]
async fn a_stopped_pass_leaves_its_events_for_the_next() {
    let test_app = app_with_weather().await;
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
    let stop = tokio_util::sync::CancellationToken::new();
    stop.cancel();
    let summary = test_app.oracle.etl_data_until(1, &stop).await.unwrap();
    assert_eq!((summary.attested, summary.failed), (0, 0));
    assert!(fetch(&test_app, event.id).await.attestation.is_none());
    test_app.run_etl().await;
    assert!(fetch(&test_app, event.id).await.attestation.is_some());
}

#[tokio::test]
async fn metrics_follow_an_event_to_its_attestation() {
    let test_app = app_with_weather().await;
    let (event, _) = event_with_entries(
        &test_app,
        vec![
            vec![pick("KORD", "temp_high", "Over")],
            vec![pick("KORD", "temp_high", "Under")],
            vec![pick("KSAW", "temp_high", "Par")],
        ],
    )
    .await;
    let text = test_app.metrics().await;
    assert_eq!(metric(&text, r#"oracle_events{state="live"}"#), 1);
    assert_eq!(metric(&text, r#"oracle_events{state="completed"}"#), 0);
    assert_eq!(metric(&text, "oracle_events_awaiting_attestation"), 0);
    assert_eq!(metric(&text, "oracle_etl_lease_held"), 0);

    // The window ended; the signing date is still ahead.
    test_app.clock.set(event.end_observation_date);
    let text = test_app.metrics().await;
    assert_eq!(metric(&text, r#"oracle_events{state="live"}"#), 0);
    assert_eq!(metric(&text, r#"oracle_events{state="completed"}"#), 1);
    assert_eq!(metric(&text, "oracle_events_awaiting_attestation"), 1);
    assert_eq!(
        metric(
            &text,
            "oracle_oldest_event_awaiting_attestation_age_seconds"
        ),
        0
    );

    // Overdue by a minute: the age shows how long.
    test_app
        .clock
        .set(event.signing_date + Duration::seconds(60));
    let text = test_app.metrics().await;
    assert_eq!(
        metric(
            &text,
            "oracle_oldest_event_awaiting_attestation_age_seconds"
        ),
        60
    );

    test_app.run_etl().await;
    let text = test_app.metrics().await;
    assert_eq!(metric(&text, r#"oracle_events{state="completed"}"#), 0);
    assert_eq!(metric(&text, r#"oracle_events{state="signed"}"#), 1);
    assert_eq!(metric(&text, "oracle_events_awaiting_attestation"), 0);
    assert_eq!(
        metric(
            &text,
            "oracle_oldest_event_awaiting_attestation_age_seconds"
        ),
        0
    );
    assert_eq!(metric(&text, "oracle_events_attested_total"), 1);
    assert_eq!(metric(&text, "oracle_event_attestation_failures_total"), 0);
    assert_eq!(
        metric(&text, r#"oracle_etl_runs_total{result="completed"}"#),
        1
    );
    assert_eq!(
        metric(&text, r#"oracle_etl_runs_total{result="failed"}"#),
        0
    );
    assert_eq!(metric(&text, "oracle_etl_lease_held"), 1);
    assert!(metric(&text, "oracle_last_etl_completed_timestamp_seconds") > 0);
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
async fn an_empty_window_blocks_instead_of_attesting_a_refund() {
    let mut weather = MockWeatherAccess::new();
    set_forecasts(&mut weather, |request, _| {
        Ok(forecasts_in_window(
            request,
            &[("KORD", 70, 50, 10), ("KSAW", 40, 30, 5)],
        ))
    });
    set_observations(&mut weather, |_, _| Ok(vec![]));
    let app = spawn_app(Arc::new(weather)).await;
    let (event, _) = event_with_entries(
        &app,
        vec![
            vec![pick("KORD", "temp_high", "Over")],
            vec![pick("KORD", "temp_high", "Under")],
            vec![pick("KORD", "temp_high", "Par")],
        ],
    )
    .await;
    app.clock.set(event.signing_date);
    let summary = app.oracle.etl_data(1).await.unwrap();
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.attested, 0);
    let blocked = fetch(&app, event.id).await;
    assert!(blocked.attestation.is_none());
    assert!(
        blocked
            .entries
            .iter()
            .all(|entry| entry.base_score.is_none())
    );
    let block = blocked.settlement_block.unwrap();
    assert_eq!(block.code, "incomplete_readings");
    assert!(block.message.contains("KORD/temp_high"));
    assert!(block.message.contains("KSAW/wind_speed"));
    let (_, body) = app.get(&format!("/events/{}", event.id)).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Settlement blocked"));
    assert!(html.contains("KORD/temp_high"));
    assert!(html.contains("role=\"alert\""));
    assert!(html.find("Settlement blocked").unwrap() < html.find("KORD/temp_high").unwrap());
    let (status, body) = app.get("/events").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8(body.to_vec())
            .unwrap()
            .contains("Settlement blocked")
    );
    for (path, target) in [
        (format!("/events/{}", event.id), "main#main-content"),
        ("/events".to_string(), "div#events-list"),
    ] {
        let (status, body) = app
            .send(
                Request::get(path)
                    .header("hx-request", "true")
                    .header("hx-target", target)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let fragment = String::from_utf8(body.to_vec()).unwrap();
        assert!(!fragment.contains("<!DOCTYPE html>"));
        assert!(fragment.contains("Settlement blocked"));
        assert!(fragment.contains("KORD/temp_high"));
    }
    let summaries: Vec<oracle::EventSummary> = app.get_json("/oracle/events").await;
    assert_eq!(summaries[0].settlement_block, Some(block));
}

/// A quality failure must preserve the previous provisional values without
/// allowing those values, or an artificial no-data refund, to be signed.
async fn quality_failure_blocks_stale_scores(rejected_reports: u64, unverified_reports: u64) {
    let phase = Arc::new(AtomicU8::new(0));
    let mut weather = MockWeatherAccess::new();
    set_forecasts(&mut weather, |request, _| {
        Ok(forecasts_in_window(
            request,
            &[("KORD", 70, 50, 10), ("KSAW", 40, 30, 5)],
        ))
    });
    let observation_phase = phase.clone();
    set_observations(&mut weather, move |_, _| {
        match observation_phase.load(Ordering::SeqCst) {
            0 => Ok(vec![
                observation("KORD", 75.0, 50.0, 10),
                observation("KSAW", 40.0, 29.4, 8),
            ]),
            1 => Err(oracle::weather_data::Error::DataQuality {
                rejected_reports,
                unverified_reports,
            }),
            _ => Ok(vec![
                observation("KORD", 65.0, 50.0, 10),
                observation("KSAW", 40.0, 29.4, 8),
            ]),
        }
    });
    let app = spawn_app(Arc::new(weather)).await;
    let (event, _) = event_with_entries(
        &app,
        vec![
            vec![pick("KORD", "temp_high", "Over")],
            vec![pick("KORD", "temp_high", "Under")],
            vec![pick("KORD", "temp_high", "Par")],
        ],
    )
    .await;
    app.clock
        .set(event.start_observation_date + Duration::hours(1));
    app.run_etl().await;
    let provisional = fetch(&app, event.id).await;
    assert_eq!(provisional.entries[0].base_score, Some(10));

    phase.store(1, Ordering::SeqCst);
    app.clock.set(event.signing_date);
    for pass in 1..=2 {
        if pass == 2 {
            // A blocked event rests between checks, then is read again.
            let resting = app.oracle.etl_data(pass).await.unwrap();
            assert_eq!((resting.failed, resting.attested), (0, 0));
            app.clock.set(event.signing_date + Duration::minutes(15));
        }
        let summary = app.oracle.etl_data(pass).await.unwrap();
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.attested, 0);
        let blocked = fetch(&app, event.id).await;
        assert!(blocked.attestation.is_none());
        assert_eq!(blocked.status, EventStatus::Completed);
        assert_eq!(blocked.readings, provisional.readings);
        assert_eq!(blocked.entries, provisional.entries);
        assert_eq!(
            blocked.settlement_block.as_ref().unwrap().code,
            "data_quality"
        );
    }

    // A later audited result must be scored again before signing. The
    // formerly losing Under entry now wins; old provisional scores cannot.
    phase.store(2, Ordering::SeqCst);
    app.clock.set(event.signing_date + Duration::minutes(30));
    let summary = app.oracle.etl_data(3).await.unwrap();
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.attested, 1);
    let signed = fetch(&app, event.id).await;
    assert_attests(&app, &signed, &[1]);
    assert!(signed.settlement_block.is_none());
}

#[tokio::test]
async fn rejected_observations_block_attestation_and_stale_scores() {
    quality_failure_blocks_stale_scores(1, 0).await;
}

#[tokio::test]
async fn unverified_observations_block_attestation_and_stale_scores() {
    quality_failure_blocks_stale_scores(0, 1).await;
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
    set_forecasts(&mut weather, |request, _| {
        if request.station_ids.contains("KMSP") {
            Err(oracle::weather_data::Error::InvalidStationId("KMSP".into()))
        } else {
            Ok(forecasts_in_window(
                request,
                &[("KORD", 70, 50, 10), ("KSAW", 40, 30, 5)],
            ))
        }
    });
    set_observations(&mut weather, |_, _| {
        Ok(vec![
            observation("KORD", 75.0, 50.0, 10),
            observation("KSAW", 40.0, 29.4, 8),
        ])
    });
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
    let summary = test_app.oracle.etl_data(1).await.unwrap();
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.attested, 1);
    assert_attests(&test_app, &fetch(&test_app, event.id).await, &[0]);
}

#[tokio::test]
async fn forecast_revisions_during_the_event_do_not_change_the_baseline_or_scores() {
    let revised = Arc::new(AtomicBool::new(false));
    let mut weather = MockWeatherAccess::new();
    let forecast_revised = revised.clone();
    set_forecasts(&mut weather, move |request, _| {
        let start = request.start.unwrap();
        // Settlement looks a week back for the latest publication before the
        // event; provisional readings only the last few hours.
        let generated_start = request.generated_start.unwrap();
        assert!(generated_start >= start - Duration::days(7) && generated_start < start);
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
        Ok(forecasts_in_window(
            request,
            &[("KORD", high, 50, 10), ("KSAW", 40, 30, 5)],
        ))
    });
    set_observations(&mut weather, |request, _| {
        let start = request.start.unwrap();
        let end = start + Duration::hours(24);
        assert!(request.end == Some(end) || request.end == Some(end - Duration::nanoseconds(1)));
        Ok(vec![
            observation("KORD", 75.0, 50.0, 10),
            observation("KSAW", 40.0, 29.4, 8),
        ])
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

#[tokio::test]
async fn a_missing_station_or_metric_cannot_be_scored_as_zero() {
    for missing_case in 0..3 {
        let mut weather = MockWeatherAccess::new();
        set_forecasts(&mut weather, |request, _| {
            Ok(forecasts_in_window(
                request,
                &[("KORD", 70, 50, 10), ("KSAW", 40, 30, 5)],
            ))
        });
        set_observations(&mut weather, move |_, _| {
            let mut rows = vec![observation("KORD", 75.0, 50.0, 10)];
            if missing_case != 0 {
                let mut station = observation("KSAW", 40.0, 29.4, 8);
                if missing_case == 1 {
                    station.wind_speed = None;
                } else {
                    station.temp_high = f64::NAN;
                }
                rows.push(station);
            }
            Ok(rows)
        });
        let app = spawn_app(Arc::new(weather)).await;
        let (event, _) = event_with_entries(
            &app,
            vec![
                vec![pick("KORD", "temp_high", "Over")],
                vec![pick("KORD", "temp_high", "Under")],
                vec![pick("KORD", "temp_high", "Par")],
            ],
        )
        .await;
        app.clock.set(event.signing_date);
        let summary = app.oracle.etl_data(1).await.unwrap();
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.attested, 0);
        let blocked = fetch(&app, event.id).await;
        assert!(blocked.attestation.is_none());
        let metric = if missing_case == 2 {
            "KSAW/temp_high"
        } else {
            "KSAW/wind_speed"
        };
        assert!(blocked.settlement_block.unwrap().message.contains(metric));
    }
}

#[tokio::test]
async fn strict_forecast_failure_preserves_provisional_scores() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_forecast_assessment()
        .returning(|request, _| {
            Ok(forecast_assessments(forecasts_in_window(
                request,
                &[("KORD", 70, 50, 10), ("KSAW", 40, 30, 5)],
            )))
        });
    weather
        .expect_settlement_forecasts()
        .returning(|_, _| Err(oracle::weather_data::Error::QualityUnavailable));
    weather.expect_observation_data().returning(|_, _| {
        Ok(vec![
            observation("KORD", 75.0, 50.0, 10),
            observation("KSAW", 40.0, 29.4, 8),
        ])
    });
    weather.expect_settlement_observations().times(0);
    let app = spawn_app(Arc::new(weather)).await;
    let (event, _) = event_with_entries(
        &app,
        vec![
            vec![pick("KORD", "temp_high", "Over")],
            vec![pick("KORD", "temp_high", "Under")],
            vec![pick("KORD", "temp_high", "Par")],
        ],
    )
    .await;
    app.clock
        .set(event.start_observation_date + Duration::hours(1));
    app.run_etl().await;
    let before = fetch(&app, event.id).await;
    app.clock.set(event.signing_date);
    let summary = app.oracle.etl_data(1).await.unwrap();
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.attested, 0);
    let after = fetch(&app, event.id).await;
    assert_eq!(after.readings, before.readings);
    assert_eq!(after.entries, before.entries);
    assert!(after.attestation.is_none());
    assert_eq!(after.settlement_block.unwrap().code, "source_unavailable");
}

#[tokio::test]
async fn verified_precipitation_remains_eligible_for_scoring() {
    let mut weather = MockWeatherAccess::new();
    set_forecasts(&mut weather, |request, _| {
        Ok(forecasts_in_window(request, &[("KORD", 70, 50, 10)])
            .into_iter()
            .map(|mut row| {
                row.rain_amt = Some(0.25);
                row
            })
            .collect())
    });
    set_observations(&mut weather, |_, _| {
        let mut row = observation("KORD", 75.0, 50.0, 10);
        row.rain_amt = Some(0.75);
        Ok(vec![row])
    });
    let app = spawn_app(Arc::new(weather)).await;
    let mut request = event_at(app.clock.now());
    // Use one UTC day so the baseline has one native daily total.
    request.start_observation_date = request
        .start_observation_date
        .date()
        .next_day()
        .unwrap()
        .with_time(Time::MIDNIGHT)
        .assume_utc();
    request.end_observation_date = request.start_observation_date + Duration::days(1);
    request.signing_date = request.end_observation_date + Duration::hours(2);
    request.locations = vec!["KORD".into()];
    request.scoring_fields = Some(vec!["rain_amt".into()]);
    request.number_of_values_per_entry = 1;
    let (status, body) = app.create_event(&request).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let event: Event = serde_json::from_slice(&body).unwrap();
    let entries = ["Over", "Under", "Par"].map(|prediction| {
        json!({
            "id": Uuid::now_v7(), "event_id": event.id,
            "picks": [pick("KORD", "rain_amt", prediction)]
        })
    });
    assert_eq!(
        app.submit_entries(event.id, json!({"event_id": event.id, "entries": entries}))
            .await
            .0,
        StatusCode::OK
    );
    app.clock.set(event.signing_date);
    let summary = app.oracle.etl_data(1).await.unwrap();
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.attested, 1);
    assert_attests(&app, &fetch(&app, event.id).await, &[0]);
}

/// A `lines` event copies the current lines when it is created and keeps
/// them. Each pick has one right answer, worth 10 points, and equal totals
/// go to the earlier entry.
#[tokio::test]
async fn lines_events_score_against_the_lines_they_were_created_with() {
    use oracle::{CreateEvent, lines::LineLevel, scoring::ScoringRules};
    let test_app = app_with_weather().await;
    let now = test_app.clock.now();
    let database = &test_app.state.database;
    database
        .replace_line_fits(
            "noaa_weather",
            24,
            vec![
                fitted_line(now, "KORD", "temp_high", -1.5, 1.5),
                fitted_line(now, "", "temp_high", -2.5, 0.5),
                fitted_line(now, "", "temp_low", -0.5, 2.5),
                fitted_line(now, "", "wind_speed", -1.5, 1.5),
            ],
        )
        .await
        .unwrap();

    let current: Value = test_app.get_json("/oracle/lines?targets=KORD,KSAW").await;
    assert_eq!(current["lines"].as_array().unwrap().len(), 6);
    assert_eq!(current["missing"], json!([]));

    let rain = CreateEvent {
        scoring_fields: Some(vec!["rain_amt".into()]),
        scoring_rules: Some(ScoringRules::Lines),
        ..event_at(now)
    };
    let (status, body) = test_app.create_event(&rain).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("rain_amt cannot be scored against lines"));

    let create = CreateEvent {
        scoring_rules: Some(ScoringRules::Lines),
        ..event_at(now)
    };
    let (status, body) = test_app.create_event(&create).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let event: Event = serde_json::from_slice(&body).unwrap();
    assert_eq!(event.scoring_rules, ScoringRules::Lines);
    assert_eq!(event.lines.len(), 6);
    let kord_high = event
        .lines
        .iter()
        .find(|line| line.target == "KORD" && line.metric == "temp_high")
        .unwrap();
    assert_eq!(kord_high.level, LineLevel::Station);
    let ksaw_high = event
        .lines
        .iter()
        .find(|line| line.target == "KSAW" && line.metric == "temp_high")
        .unwrap();
    assert_eq!(ksaw_high.level, LineLevel::Pooled);
    assert_eq!((ksaw_high.lower, ksaw_high.upper), (-2.5, 0.5));

    // Refitting afterwards does not move the event's lines.
    database
        .replace_line_fits(
            "noaa_weather",
            24,
            vec![fitted_line(now, "", "temp_high", -9.5, 9.5)],
        )
        .await
        .unwrap();
    assert_eq!(fetch(&test_app, event.id).await.lines, event.lines);

    // KORD misses +5 / 0 / 0 and KSAW 0 / -0.6 / +3 (high / low / wind).
    let ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
    let picks = [
        // 10 (+5 is over 1.5) + 10 (-0.6 is under -0.5) = 20
        vec![
            pick("KORD", "temp_high", "Over"),
            pick("KSAW", "temp_low", "Under"),
        ],
        // 0 (not par) + 10 (0 is par) = 10
        vec![
            pick("KORD", "temp_high", "Par"),
            pick("KORD", "wind_speed", "Par"),
        ],
        // 10 + 10 = 20, the same as the first, which entered earlier
        vec![
            pick("KSAW", "wind_speed", "Over"),
            pick("KSAW", "temp_high", "Par"),
        ],
    ];
    let entries: Vec<Value> = ids
        .iter()
        .zip(picks)
        .map(|(id, picks)| json!({"id": id, "event_id": event.id, "picks": picks}))
        .collect();
    let (status, body) = test_app
        .submit_entries(event.id, json!({"event_id": event.id, "entries": entries}))
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    test_app
        .clock
        .set(event.start_observation_date + Duration::hours(1));
    test_app.run_etl().await;
    let running = fetch(&test_app, event.id).await;
    let base_scores: Vec<Option<i64>> = running.entries.iter().map(|e| e.base_score).collect();
    assert_eq!(base_scores, vec![Some(20), Some(10), Some(20)]);

    test_app.clock.set(event.signing_date);
    test_app.run_etl().await;
    let signed = fetch(&test_app, event.id).await;
    assert_eq!(signed.status, EventStatus::Signed);
    assert_attests(&test_app, &signed, &[0]);
}

/// Without a fitted line, and no pooled one, a `lines` event is refused
/// rather than scored some other way.
#[tokio::test]
async fn lines_events_need_a_line_for_every_pick() {
    use oracle::{CreateEvent, scoring::ScoringRules};
    let test_app = app_with_weather().await;
    let now = test_app.clock.now();
    test_app
        .state
        .database
        .replace_line_fits(
            "noaa_weather",
            24,
            vec![fitted_line(now, "", "temp_high", -1.5, 1.5)],
        )
        .await
        .unwrap();
    let create = CreateEvent {
        scoring_rules: Some(ScoringRules::Lines),
        ..event_at(now)
    };
    let (status, body) = test_app.create_event(&create).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("KORD/temp_low"), "{body}");
    assert!(!body.contains("temp_high"), "{body}");

    let current: Value = test_app
        .get_json("/oracle/lines?targets=KORD&metrics=temp_high,wind_speed")
        .await;
    assert_eq!(current["missing"], json!(["KORD/wind_speed"]));

    // Events that do not ask for lines keep the fixed rules.
    let (status, body) = test_app.create_event(&event_at(now)).await;
    assert_eq!(status, StatusCode::OK);
    let event: Event = serde_json::from_slice(&body).unwrap();
    assert_eq!(event.scoring_rules, ScoringRules::Fixed);
    assert!(event.lines.is_empty());
}
