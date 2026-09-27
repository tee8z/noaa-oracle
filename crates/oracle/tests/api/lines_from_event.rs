//! `lines_from_event`: events created at different times, such as the pools
//! of one competition, freeze the same lines by copying an earlier event's
//! instead of the current fit.

use crate::helpers::{MockWeatherAccess, TestApp, event_at, fitted_line, signed, spawn_app};
use axum::http::{Method, StatusCode};
use dlctix::musig2::secp256k1::PublicKey;
use nostr::key::Keys;
use oracle::{CreateEvent, Event, lines::Line, scoring::ScoringRules, statement::Terms};
use serde_json::{Value, json};
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

async fn app() -> TestApp {
    spawn_app(Arc::new(MockWeatherAccess::new())).await
}

/// Replaces the stored 24-hour fit.
async fn fit(test_app: &TestApp, lines: Vec<Line>) {
    test_app
        .state
        .database
        .replace_line_fits("noaa_weather", 24, lines)
        .await
        .unwrap();
}

/// A fit with a line for KORD and KSAW and every default metric. Most cuts
/// have no short decimal form, so a copy that rounds them would show.
fn first_fit(now: OffsetDateTime) -> Vec<Line> {
    vec![
        fitted_line(now, "KORD", "temp_high", -1.0 / 3.0, 0.1 + 0.2),
        fitted_line(now, "", "temp_high", -2.5, 2.0_f64.sqrt()),
        fitted_line(now, "", "temp_low", -0.7, 1.0 / 7.0),
        fitted_line(now, "", "wind_speed", -1.5, 1.5),
    ]
}

/// A `lines` event body over KORD and KSAW, as a coordinator posts it,
/// copying `from`'s lines when given.
fn lines_event(test_app: &TestApp, from: Option<Uuid>) -> Value {
    let mut body = serde_json::to_value(CreateEvent {
        scoring_rules: Some(ScoringRules::Lines),
        ..event_at(test_app.clock.now())
    })
    .unwrap();
    if let Some(from) = from {
        body["lines_from_event"] = json!(from);
    }
    body
}

async fn post(test_app: &TestApp, keys: &Keys, body: &Value) -> (StatusCode, Value) {
    let (status, response) = test_app
        .send(signed(
            Method::POST,
            "/oracle/events",
            body.to_string().into_bytes(),
            keys,
        ))
        .await;
    (status, serde_json::from_slice(&response).unwrap())
}

async fn create(test_app: &TestApp, body: &Value) -> Event {
    let (status, response) = post(test_app, &test_app.coordinator, body).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    serde_json::from_value(response).unwrap()
}

/// The error of a refused create; checks nothing was stored.
async fn refused(test_app: &TestApp, body: &Value) -> String {
    let (status, response) = post(test_app, &test_app.coordinator, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    let id = body["id"].as_str().unwrap();
    let (status, _) = test_app.get(&format!("/oracle/events/{id}")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a refused event is not stored"
    );
    response["error"].as_str().unwrap().to_owned()
}

/// The bands' exact bits, by target and metric.
fn bits(lines: &[Line]) -> Vec<(String, String, u64, u64)> {
    lines
        .iter()
        .map(|line| {
            (
                line.target.clone(),
                line.metric.clone(),
                line.lower.to_bits(),
                line.upper.to_bits(),
            )
        })
        .collect()
}

#[tokio::test]
async fn events_freeze_the_lines_of_the_event_they_name_not_the_current_fit() {
    let test_app = app().await;
    let now = test_app.clock.now();
    fit(&test_app, first_fit(now)).await;
    let first = create(&test_app, &lines_event(&test_app, None)).await;
    assert_eq!(first.lines.len(), 6);

    // The lines refit before the next event is created.
    let later = now + Duration::hours(12);
    test_app.clock.set(later);
    fit(
        &test_app,
        ["temp_high", "temp_low", "wind_speed"]
            .map(|metric| fitted_line(later, "", metric, -0.5, 0.5))
            .to_vec(),
    )
    .await;
    let current: Value = test_app.get_json("/oracle/lines?targets=KORD,KSAW").await;
    let current: Vec<Line> = serde_json::from_value(current["lines"].clone()).unwrap();
    assert_eq!(current.len(), 6);
    assert!(
        current.iter().all(|line| !first.lines.contains(line)),
        "the current fit differs from the first event's lines"
    );

    let pool = create(&test_app, &lines_event(&test_app, Some(first.id))).await;
    assert_eq!(pool.scoring_rules, ScoringRules::Lines);
    assert_eq!(pool.lines, first.lines, "every stored field, fitted_at too");
    assert_eq!(bits(&pool.lines), bits(&first.lines));
    let fetched: Event = test_app
        .get_json(&format!("/oracle/events/{}", pool.id))
        .await;
    assert_eq!(fetched.lines, first.lines);
    assert_eq!(bits(&fetched.lines), bits(&first.lines));

    // An event scoring fewer targets and metrics copies only those lines.
    let mut smaller = lines_event(&test_app, Some(first.id));
    smaller["locations"] = json!(["KSAW"]);
    smaller["scoring_fields"] = json!(["temp_high"]);
    smaller["number_of_values_per_entry"] = json!(1);
    let smaller = create(&test_app, &smaller).await;
    let ksaw_high: Vec<Line> = first
        .lines
        .iter()
        .filter(|line| line.target == "KSAW" && line.metric == "temp_high")
        .cloned()
        .collect();
    assert_eq!(ksaw_high.len(), 1);
    assert_eq!(smaller.lines, ksaw_high);

    // Once the entries are in, the signed statement carries the same lines.
    let entries: Vec<Value> = ["Over", "Par", "Under"]
        .iter()
        .map(|prediction| {
            json!({
                "id": Uuid::now_v7(),
                "event_id": pool.id,
                "picks": [{"target": "KORD", "metric": "temp_high", "prediction": prediction}]
            })
        })
        .collect();
    let (status, response) = test_app
        .submit_entries(pool.id, json!({"event_id": pool.id, "entries": entries}))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&response)
    );
    let entered: Event = test_app
        .get_json(&format!("/oracle/events/{}", pool.id))
        .await;
    let statement = entered
        .statement
        .expect("a statement once every entry is in");
    let key = PublicKey::from_slice(&test_app.oracle.public_key().serialize())
        .unwrap()
        .x_only_public_key()
        .0;
    assert_eq!(statement.verify(&key), Ok(()));
    let Terms::Observation(terms) = &statement.statement.terms else {
        panic!("weather events are observation events");
    };
    let mut expected: Vec<(String, String, u64, u64, u32)> = first
        .lines
        .iter()
        .map(|line| {
            (
                line.target.clone(),
                line.metric.clone(),
                line.lower.to_bits(),
                line.upper.to_bits(),
                u32::try_from(line.window_hours).unwrap(),
            )
        })
        .collect();
    expected.sort();
    let signed_lines: Vec<(String, String, u64, u64, u32)> = terms
        .lines
        .iter()
        .map(|line| {
            (
                line.target.clone(),
                line.metric.clone(),
                line.lower.to_bits(),
                line.upper.to_bits(),
                line.window_hours,
            )
        })
        .collect();
    assert_eq!(signed_lines, expected);
}

#[tokio::test]
async fn lines_from_event_is_refused_unless_the_named_event_can_lend_its_lines() {
    let test_app = app().await;
    let now = test_app.clock.now();
    fit(&test_app, first_fit(now)).await;
    let first = create(&test_app, &lines_event(&test_app, None)).await;

    // The new event must score against lines itself.
    let mut fixed_rules = lines_event(&test_app, Some(first.id));
    fixed_rules["scoring_rules"] = json!("fixed");
    assert_eq!(
        refused(&test_app, &fixed_rules).await,
        "lines_from_event needs lines scoring rules"
    );

    let unknown = Uuid::now_v7();
    assert_eq!(
        refused(&test_app, &lines_event(&test_app, Some(unknown))).await,
        format!("lines_from_event {unknown} is not an event on this oracle")
    );

    let theirs = lines_event(&test_app, None);
    let (status, response) = post(&test_app, &test_app.other_coordinator, &theirs).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let theirs: Event = serde_json::from_value(response).unwrap();
    assert_eq!(
        refused(&test_app, &lines_event(&test_app, Some(theirs.id))).await,
        format!(
            "lines_from_event {} was created by another coordinator",
            theirs.id
        )
    );

    let mut fixed = lines_event(&test_app, None);
    fixed["scoring_rules"] = json!("fixed");
    let fixed = create(&test_app, &fixed).await;
    assert!(fixed.lines.is_empty());
    assert_eq!(
        refused(&test_app, &lines_event(&test_app, Some(fixed.id))).await,
        format!(
            "lines_from_event {} does not use lines scoring rules",
            fixed.id
        )
    );

    // The named event scores no wind, so it froze no wind lines.
    let mut no_wind = lines_event(&test_app, None);
    no_wind["scoring_fields"] = json!(["temp_high", "temp_low"]);
    let no_wind = create(&test_app, &no_wind).await;
    assert_eq!(no_wind.lines.len(), 4);
    assert_eq!(
        refused(&test_app, &lines_event(&test_app, Some(no_wind.id))).await,
        format!(
            "lines_from_event {} has no line fitted on this event's window length \
             for KORD/wind_speed, KSAW/wind_speed",
            no_wind.id
        )
    );

    // The same event can lend its lines to the events that need no more.
    let mut two_metrics = lines_event(&test_app, Some(no_wind.id));
    two_metrics["scoring_fields"] = json!(["temp_low", "temp_high"]);
    assert_eq!(create(&test_app, &two_metrics).await.lines, no_wind.lines);
}
