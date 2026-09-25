//! Event creation, entry submission, listing, and source discovery over
//! HTTP. Rule details are unit tested in `events.rs`; these prove the
//! routes apply them with the right status codes.

use crate::helpers::{MockWeatherAccess, TestApp, event_at, signed, spawn_app};
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use nostr::key::Keys;
use oracle::{CreateEvent, Event, EventStatus, EventSummary, WeatherEntry};
use serde_json::{Value, json};
use std::sync::Arc;
use time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

async fn app() -> TestApp {
    spawn_app(Arc::new(MockWeatherAccess::new())).await
}

async fn created(test_app: &TestApp) -> Event {
    let (status, body) = test_app.create_event(&event_at(test_app.clock.now())).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

fn picks_entry(event_id: Uuid, station: &str, prediction: &str) -> Value {
    json!({
        "id": Uuid::now_v7(),
        "event_id": event_id,
        "picks": [{"target": station, "metric": "temp_high", "prediction": prediction}]
    })
}

fn choices_entry(event_id: Uuid, station: &str) -> Value {
    json!({
        "id": Uuid::now_v7(),
        "event_id": event_id,
        "expected_observations": [{"stations": station, "temp_low": "Under", "wind_speed": "Par"}]
    })
}

#[tokio::test]
async fn created_events_announce_every_outcome_with_a_public_nonce_point() {
    let test_app = app().await;
    let event = created(&test_app).await;
    assert_eq!(event.status, EventStatus::Live);
    assert_eq!(event.source, "noaa_weather");
    // 3 entries, 1 place: three winner outcomes and the refund outcome.
    assert_eq!(event.event_announcement.locking_points.len(), 4);
    let fetched: Event = test_app
        .get_json(&format!("/oracle/events/{}", event.id))
        .await;
    assert_eq!(fetched.nonce_point, event.nonce_point);
    let other = created(&test_app).await;
    assert_ne!(other.nonce_point, event.nonce_point, "nonces are per event");
}

#[tokio::test]
async fn invalid_events_are_bad_requests() {
    let test_app = app().await;
    let now = test_app.clock.now();
    let mut too_many_outcomes = event_at(now);
    too_many_outcomes.total_allowed_entries = 25;
    too_many_outcomes.number_of_places_win = 5;
    let mut unknown_source = event_at(now);
    unknown_source.source = Some("space_weather".into());
    let mut unknown_metric = event_at(now);
    unknown_metric.scoring_fields = Some(vec!["dew_point".into()]);
    let mut places_equal_entries = event_at(now);
    places_equal_entries.number_of_places_win = 3;
    for event in [
        too_many_outcomes,
        unknown_source,
        unknown_metric,
        places_equal_entries,
    ] {
        let (status, body) = test_app.create_event(&event).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }
}

#[tokio::test]
async fn entries_accept_picks_or_weather_choices_once() {
    let test_app = app().await;
    let event = created(&test_app).await;
    let body = json!({
        "event_id": event.id,
        "entries": [
            picks_entry(event.id, "KORD", "Over"),
            picks_entry(event.id, "KSAW", "Par"),
            choices_entry(event.id, "KORD"),
        ]
    });
    let (status, response) = test_app.submit_entries(event.id, body.clone()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&response)
    );
    let entries: Vec<WeatherEntry> = serde_json::from_slice(&response).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2].picks.len(), 2, "weather choices become picks");

    let fetched: Event = test_app
        .get_json(&format!("/oracle/events/{}", event.id))
        .await;
    let mut ids: Vec<Uuid> = entries.iter().map(|entry| entry.id).collect();
    ids.sort();
    assert_eq!(fetched.entry_ids, ids, "entries are listed in id order");
    let entry: WeatherEntry = test_app
        .get_json(&format!("/oracle/events/{}/entries/{}", event.id, ids[0]))
        .await;
    assert_eq!(entry.id, ids[0]);

    let again = json!({
        "event_id": event.id,
        "entries": [
            picks_entry(event.id, "KORD", "Over"),
            picks_entry(event.id, "KSAW", "Par"),
            picks_entry(event.id, "KSAW", "Under"),
        ]
    });
    assert_eq!(
        test_app.submit_entries(event.id, again).await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn entries_are_closed_after_the_observation_window() {
    let test_app = app().await;
    let event = created(&test_app).await;
    test_app.clock.set(event.end_observation_date);
    let body = json!({
        "event_id": event.id,
        "entries": [
            picks_entry(event.id, "KORD", "Over"),
            picks_entry(event.id, "KSAW", "Par"),
            picks_entry(event.id, "KSAW", "Under"),
        ]
    });
    assert_eq!(
        test_app.submit_entries(event.id, body).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn invalid_entries_are_rejected() {
    let test_app = app().await;
    let event = created(&test_app).await;
    let submit = |entries: Vec<Value>| {
        test_app.submit_entries(event.id, json!({"event_id": event.id, "entries": entries}))
    };
    let unknown_station = vec![
        picks_entry(event.id, "KMSP", "Over"),
        picks_entry(event.id, "KSAW", "Par"),
        picks_entry(event.id, "KSAW", "Under"),
    ];
    assert_eq!(submit(unknown_station).await.0, StatusCode::BAD_REQUEST);
    let too_few = vec![picks_entry(event.id, "KORD", "Over")];
    assert_eq!(submit(too_few).await.0, StatusCode::BAD_REQUEST);
    let mut both_forms = choices_entry(event.id, "KORD");
    both_forms["picks"] = picks_entry(event.id, "KORD", "Over")["picks"].clone();
    let both = vec![
        both_forms,
        picks_entry(event.id, "KSAW", "Par"),
        picks_entry(event.id, "KSAW", "Under"),
    ];
    assert_eq!(submit(both).await.0, StatusCode::BAD_REQUEST);
    let (status, _) = test_app
        .submit_entries(Uuid::now_v7(), json!({"event_id": event.id, "entries": []}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "path and body must agree");
}

#[tokio::test]
async fn only_the_events_coordinator_submits_entries() {
    let test_app = app().await;
    let event = created(&test_app).await;
    let body = json!({
        "event_id": event.id,
        "entries": [
            picks_entry(event.id, "KORD", "Over"),
            picks_entry(event.id, "KSAW", "Par"),
            picks_entry(event.id, "KSAW", "Under"),
        ]
    });
    let request = signed(
        Method::POST,
        &format!("/oracle/events/{}/entries", event.id),
        serde_json::to_vec(&body).unwrap(),
        &test_app.other_coordinator,
    );
    assert_eq!(test_app.send(request).await.0, StatusCode::FORBIDDEN);
    let stranger = signed(
        Method::POST,
        &format!("/oracle/events/{}/entries", event.id),
        serde_json::to_vec(&body).unwrap(),
        &Keys::generate(),
    );
    assert_eq!(test_app.send(stranger).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn listing_is_newest_first_bounded_and_filterable() {
    let test_app = app().await;
    let first = created(&test_app).await;
    let second = created(&test_app).await;
    let all: Vec<EventSummary> = test_app.get_json("/oracle/events").await;
    assert_eq!(
        all.iter().map(|event| event.id).collect::<Vec<_>>(),
        vec![second.id, first.id]
    );
    let limited: Vec<EventSummary> = test_app.get_json("/oracle/events?limit=1").await;
    assert_eq!(limited.len(), 1);
    let clamped: Vec<EventSummary> = test_app.get_json("/oracle/events?limit=100000").await;
    assert_eq!(clamped.len(), 2);
    let filtered: Vec<EventSummary> = test_app
        .get_json(&format!("/oracle/events?event_ids={}", first.id))
        .await;
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, first.id);
    assert_eq!(
        test_app.get("/oracle/events?event_ids=nope").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        test_app
            .get(&format!("/oracle/events/{}", Uuid::now_v7()))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let later = test_app.clock.now() + Duration::days(3);
    test_app.clock.set(later);
    let completed: Vec<EventSummary> = test_app.get_json("/oracle/events").await;
    assert!(
        completed
            .iter()
            .all(|event| event.status == EventStatus::Completed)
    );
}

async fn created_with(test_app: &TestApp, unlisted: bool) -> Event {
    let event = CreateEvent {
        unlisted,
        ..event_at(test_app.clock.now())
    };
    let (status, body) = test_app.create_event(&event).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

async fn html(test_app: &TestApp, path: &str) -> String {
    let (status, body) = test_app.get(path).await;
    assert_eq!(status, StatusCode::OK, "{path}");
    String::from_utf8(body.to_vec()).unwrap()
}

fn rows(html: &str) -> usize {
    html.matches("<a class=\"ev-row\" href=").count()
}

/// Unlisted events stay off the events list and the counts, filtered
/// before the page limit, until the reader asks for them. Their own page
/// and the API still serve them.
#[tokio::test]
async fn unlisted_events_stay_off_the_list_and_counts_but_open_by_their_link() {
    let mut weather = MockWeatherAccess::new();
    weather
        .expect_observation_data()
        .returning(|_, _| Ok(vec![]));
    weather
        .expect_daily_observations()
        .returning(|_, _| Ok(vec![]));
    weather.expect_forecasts_data().returning(|_, _| Ok(vec![]));
    weather.expect_stations().returning(|| Ok(vec![]));
    let test_app = spawn_app(Arc::new(weather)).await;

    // One listed event, then a full page (50 rows) of newer unlisted ones.
    let listed = created_with(&test_app, false).await;
    assert!(!listed.unlisted);
    let mut unlisted = vec![];
    for _ in 0..50 {
        unlisted.push(created_with(&test_app, true).await);
    }
    assert!(unlisted.iter().all(|event| event.unlisted));
    let newest = unlisted.last().unwrap().id.to_string();
    let oldest_unlisted = unlisted[0].id;

    // Listed only: the old listed event is on the first page, and the
    // counts and the toggle agree with it.
    let page = html(&test_app, "/events").await;
    assert_eq!(rows(&page), 1);
    assert!(page.contains(&listed.id.to_string()));
    assert!(!page.contains(&newest));
    assert!(page.contains("All <span class=\"chip-count\">1</span>"));
    assert!(page.contains("Live <span class=\"chip-count\">1</span>"));
    assert!(page.contains("Show unlisted"));
    assert!(page.contains("(50 hidden)"));
    assert!(!page.contains("Older events"));
    let dashboard = html(&test_app, "/").await;
    assert!(
        dashboard.contains("<span class=\"stat-value stat-live\">1</span>"),
        "{dashboard}"
    );

    // With the toggle: newest first, a page at a time, counted too.
    let page = html(&test_app, "/events?unlisted=show").await;
    assert_eq!(rows(&page), 50);
    assert!(page.contains(&newest));
    assert!(!page.contains(&listed.id.to_string()));
    assert!(page.contains("All <span class=\"chip-count\">51</span>"));
    assert!(!page.contains("hidden)"));
    assert!(page.contains(">Unlisted<"));
    let older = format!("/events?unlisted=show&before={oldest_unlisted}");
    assert!(page.contains(&older.replace('&', "&amp;")), "{page}");
    let page = html(&test_app, &older).await;
    assert_eq!(rows(&page), 1);
    assert!(page.contains(&listed.id.to_string()));
    assert!(page.contains("Newest events"));

    // Reachable by their link, on their page and over the API.
    let page = html(&test_app, &format!("/events/{newest}")).await;
    assert!(page.contains(&newest));
    assert!(page.contains(">Unlisted<"));
    let fetched: Event = test_app.get_json(&format!("/oracle/events/{newest}")).await;
    assert!(fetched.unlisted);
    let summaries: Vec<EventSummary> = test_app
        .get_json(&format!("/oracle/events?event_ids={newest},{}", listed.id))
        .await;
    assert_eq!(
        summaries
            .iter()
            .map(|event| (event.id, event.unlisted))
            .collect::<Vec<_>>(),
        vec![(unlisted[49].id, true), (listed.id, false)]
    );
}

#[tokio::test]
async fn sources_describe_metrics_and_par_rules() {
    let test_app = app().await;
    let sources: Value = test_app.get_json("/oracle/sources").await;
    let noaa = &sources[0];
    assert_eq!(noaa["id"], "noaa_weather");
    assert_eq!(noaa["default"], true);
    assert_eq!(
        noaa["default_metrics"],
        json!(["temp_high", "temp_low", "wind_speed"])
    );
    let rain = noaa["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|metric| metric["id"] == "rain_amt")
        .unwrap();
    assert_eq!(rain["par"], json!({"rule": "within", "tolerance": 0.1}));
}

#[tokio::test]
async fn oracle_identity_is_public() {
    let test_app = app().await;
    let npub: Value = test_app.get_json("/oracle/npub").await;
    assert!(npub["key"].as_str().unwrap().starts_with("npub1"));
    let pubkey: Value = test_app.get_json("/oracle/pubkey").await;
    assert_eq!(pubkey["key"], test_app.oracle.public_key_base64());
}

#[tokio::test]
async fn event_pages_return_only_their_content_to_htmx() {
    let test_app = app().await;
    let event = created(&test_app).await;
    for path in [format!("/events/{}", event.id), "/events".to_string()] {
        let (status, page) = test_app.get(&path).await;
        assert_eq!(status, StatusCode::OK);
        let page = String::from_utf8(page.to_vec()).unwrap();
        assert!(page.starts_with("<!DOCTYPE html>"), "{path}");
        assert_eq!(page.matches("class=\"site-header\"").count(), 1);

        let request = Request::get(&path)
            .header("hx-request", "true")
            .header("hx-target", "main#main-content")
            .body(Body::empty())
            .unwrap();
        let response = test_app.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::VARY],
            "HX-Request, HX-Target, HX-History-Restore-Request, Cookie"
        );
        let fragment = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let fragment = String::from_utf8(fragment.to_vec()).unwrap();
        assert!(!fragment.contains("<html"), "{path}");
        assert!(!fragment.contains("site-header"), "{path}");
        // The tabs come along out of band so the current page stays marked.
        assert!(fragment.contains("id=\"site-tabs\""), "{path}");
        assert!(fragment.contains("hx-swap-oob=\"true\""), "{path}");
        assert!(fragment.contains("aria-current=\"page\""), "{path}");
        assert!(fragment.contains(&event.id.to_string()[..8]), "{path}");

        // htmx restores history by replacing the body, so it needs the page.
        let (_, restored) = test_app
            .send(
                Request::get(&path)
                    .header("hx-request", "true")
                    .header("hx-target", "body")
                    .header("hx-history-restore-request", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert!(restored.starts_with(b"<!DOCTYPE html>"), "{path}");
    }
}

/// Filters replace the section, the refresh replaces only the rows, and
/// both keep the filters.
#[tokio::test]
async fn event_filters_render_the_matching_part() {
    let test_app = app().await;
    let event = created(&test_app).await;
    let short_id = &event.id.to_string()[..8];

    let part = |target: &'static str, path: &'static str| {
        Request::get(path)
            .header("hx-request", "true")
            .header("hx-target", target)
            .body(Body::empty())
            .unwrap()
    };
    let (_, body) = test_app
        .send(part("section#events", "/events?status=live"))
        .await;
    let section = String::from_utf8(body.to_vec()).unwrap();
    assert!(section.starts_with("<section id=\"events\""), "{section}");
    assert!(section.contains(short_id));
    assert!(section.contains("hx-get=\"/events?status=live\""));

    let (_, body) = test_app
        .send(part("div#events-list", "/events?status=signed"))
        .await;
    let list = String::from_utf8(body.to_vec()).unwrap();
    assert!(list.starts_with("<div id=\"events-list\""), "{list}");
    assert!(!list.contains(short_id));
    assert!(list.contains("No events match these filters."));
    assert!(!list.contains("status-filter"));
}

/// The raw data page needs its own policy and script, so htmx never swaps
/// it into another page: going back to it reloads it.
#[tokio::test]
async fn htmx_reloads_the_raw_data_page_instead_of_swapping_it() {
    let test_app = app().await;
    let request = Request::get("/raw")
        .header("hx-request", "true")
        .header("hx-target", "body")
        .header("hx-history-restore-request", "true")
        .body(Body::empty())
        .unwrap();
    let response = test_app.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["hx-refresh"], "true");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(body.is_empty());
}

/// Attributes named `on…`, which run inline script.
fn inline_handlers(html: &str) -> Vec<&str> {
    html.match_indices(" on")
        .filter_map(|(at, _)| {
            let name = &html[at + 1..];
            let end = name.find(|c: char| !c.is_ascii_lowercase())?;
            (end > 2 && name[end..].starts_with('=')).then_some(&name[..end])
        })
        .collect()
}

/// Pages send a Content-Security-Policy that allows only this site's script
/// files, and contain nothing it would block: every script is a file, no
/// element has an inline handler, and htmx is told not to evaluate code.
#[tokio::test]
async fn pages_allow_only_their_own_script_files() {
    let test_app = app().await;
    let event = created(&test_app).await;
    for (path, status, extra) in [
        ("/events".to_string(), StatusCode::OK, None),
        (format!("/events/{}", event.id), StatusCode::OK, None),
        (
            format!("/events/{}", Uuid::nil()),
            StatusCode::NOT_FOUND,
            None,
        ),
        (
            "/raw".to_string(),
            StatusCode::OK,
            Some(
                "'wasm-unsafe-eval' https://cdn.jsdelivr.net/npm/@duckdb/duckdb-wasm@1.29.0/ \
                 https://cdn.jsdelivr.net/npm/apache-arrow@17.0.0/+esm",
            ),
        ),
    ] {
        let response = test_app
            .app
            .clone()
            .oneshot(Request::get(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
        let policy = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(policy.starts_with("script-src 'self'"), "{path}: {policy}");
        for directive in [
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
        ] {
            assert!(policy.contains(directive), "{path}: {policy}");
        }
        assert!(!policy.contains("unsafe-inline"), "{path}: {policy}");
        assert!(!policy.contains("'unsafe-eval'"), "{path}: {policy}");
        match extra {
            Some(sources) => {
                assert!(policy.contains(sources), "{path}: {policy}");
                // Only the exact modules, not the whole CDN.
                assert!(!policy.contains("jsdelivr.net "), "{path}: {policy}");
                assert!(!policy.contains("jsdelivr.net;"), "{path}: {policy}");
            }
            None => {
                assert!(!policy.contains("jsdelivr"), "{path}: {policy}");
                assert!(
                    policy.contains("require-trusted-types-for 'script'; trusted-types htmx"),
                    "{path}: {policy}"
                );
            }
        }

        let html = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(html.to_vec()).unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"), "{path}");
        for script in html.split("<script").skip(1) {
            let (attributes, rest) = script.split_once('>').unwrap();
            assert!(
                attributes.contains(" src=\"/assets/"),
                "{path}: <script{attributes}>"
            );
            assert!(rest.starts_with("</script>"), "{path}: inline script");
        }
        assert_eq!(inline_handlers(&html), Vec::<&str>::new(), "{path}");
        assert!(!html.contains("hx-on"), "{path}");
        // htmx 4 has no switch for eval; it may only call this site.
        assert!(
            html.contains("&quot;mode&quot;:&quot;same-origin&quot;"),
            "{path}"
        );
        assert!(html.contains("&quot;defaultTimeout&quot;:10000"), "{path}");
        // Only the raw data page loads its DuckDB script.
        assert_eq!(html.contains("/assets/raw-data."), path == "/raw", "{path}");
    }
    assert_eq!(inline_handlers(r#"<a onclick="x()">once</a>"#), ["onclick"]);
    assert!(inline_handlers(r#"<p hx-trigger="toggle once">on = off</p>"#).is_empty());
}
