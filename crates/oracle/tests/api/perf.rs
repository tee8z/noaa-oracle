//! Page, fragment and API timings on a copy of real weather data. Not run
//! by default; point `ORACLE_PERF_DATA` at a weather directory:
//!
//! ```text
//! ORACLE_PERF_DATA=~/weather cargo test -p oracle --test api perf -- --ignored --nocapture
//! ```
//!
//! Each request runs three times. The first run is cold: the station list
//! and the map-popup cache start empty.

use crate::helpers::{event_at, spawn_app};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use oracle::{
    file_access::FileAccess,
    weather_data::{WeatherAccess, WeatherData},
};
use std::{sync::Arc, time::Instant};
use time::{Duration, OffsetDateTime, Time, UtcOffset, format_description::well_known::Rfc3339};

const STATIONS: &str = "KPWM,KBTV,KBED";

fn at(time: OffsetDateTime) -> String {
    time.to_offset(UtcOffset::UTC).format(&Rfc3339).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ORACLE_PERF_DATA"]
async fn page_timings_on_real_data() {
    let Ok(directory) = std::env::var("ORACLE_PERF_DATA") else {
        eprintln!("ORACLE_PERF_DATA is not set");
        return;
    };
    let weather = Arc::new(WeatherAccess::with_derived_forecasts(
        Arc::new(FileAccess::new(directory.clone())),
        &std::path::Path::new(&directory).join("derived"),
    ));
    weather
        .prepare_files(&tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let test_app = spawn_app(weather).await;

    let now = OffsetDateTime::now_utc();
    let event = event_at(now);
    assert!(test_app.create_event(&event).await.0.is_success());
    let today = now.replace_time(Time::MIDNIGHT);
    // A finished 24-hour competition window, and one that started at 02:00
    // today and is still running.
    let finished = today - Duration::hours(21);
    let running = today + Duration::hours(2);
    let airports = "KATL,KLAX,KORD,KDFW,KDEN,KJFK,KSFO,KSEA,KLAS,KMCO,KEWR,KMIA,KPHX,KIAH,\
        KBOS,KMSP,KFLL,KDTW,KPHL,KLGA,KBWI,KSLC,KDCA,KSAN,KTPA,KPDX,KSTL,KHNL,KBNA,KAUS,KMCI,\
        KRDU,KMKE,KSMF,KCLT,KPIT,KSAT,KOAK,KCLE,KSJC,KIND,KCVG,KCMH,KJAN,KRSW,KABQ,KANC,KOMA,\
        KBUF,KPBI,KBDL,KPVD,KBTV,KPWM,KMHT,KBOI,KBIL,KFSD,KFAR,KGEG,KICT,KLIT,KLEX,KBHM,KMEM,\
        KJAX,KCHS,KRIC,KORF,KCRW,KPNS,KMOB,KSHV,KMSY,KTUL,KELP,KTUS,KCOS,KGRR,KDSM,KMSN,KDLH,\
        KBZN,KGJT,KRAP,KFCA,KCYS,KJAR,KSGF,KFSM";
    let requests: Vec<(&str, String, bool)> = vec![
        ("station list", "/stations".into(), false),
        ("events page", "/events".into(), false),
        ("events content", "/events".into(), true),
        ("event detail page", format!("/events/{}", event.id), false),
        (
            "event detail content",
            format!("/events/{}", event.id),
            true,
        ),
        ("raw data page", "/raw".into(), false),
        ("raw data content", "/raw".into(), true),
        ("oracle info", "/fragments/oracle-info".into(), true),
        ("event stats", "/fragments/event-stats".into(), true),
        ("dashboard page", "/".into(), false),
        ("dashboard content (htmx)", "/".into(), true),
        ("weather map", "/fragments/weather?view=map".into(), true),
        ("weather list", "/fragments/weather?view=list".into(), true),
        ("map station popup", "/fragments/station/KORD".into(), true),
        ("map popup KORD", "/fragments/forecast/KORD".into(), true),
        ("map popup KSAW", "/fragments/forecast/KSAW".into(), true),
        (
            "API forecasts, entry form (3 stations, next 2 days)",
            format!(
                "/stations/forecasts?station_ids={STATIONS}&start={}&end={}",
                at(now),
                at(now + Duration::days(2))
            ),
            false,
        ),
        (
            "API forecasts, leaderboard (3 stations, finished window)",
            format!(
                "/stations/forecasts?station_ids={STATIONS}&start={}&end={}",
                at(finished),
                at(finished + Duration::days(1))
            ),
            false,
        ),
        (
            "API forecasts, 90 airports today",
            format!(
                "/stations/forecasts?station_ids={airports}&start={}&end={}",
                at(today),
                at(today + Duration::days(1))
            ),
            false,
        ),
        (
            "API observations, leaderboard (finished window)",
            format!(
                "/stations/observations?station_ids={STATIONS}&start={}&end={}",
                at(finished),
                at(finished + Duration::days(1))
            ),
            false,
        ),
        (
            "API observations, live progress (window start to now)",
            format!(
                "/stations/observations?station_ids={STATIONS}&start={}&end={}",
                at(running),
                at(now)
            ),
            false,
        ),
        (
            "API observations, 90 airports today",
            format!(
                "/stations/observations?station_ids={airports}&start={}&end={}",
                at(today),
                at(now)
            ),
            false,
        ),
        (
            "API daily observations (3 stations, 7 days)",
            format!(
                "/stations/daily-observations?station_ids={STATIONS}&start={}&end={}",
                at(today - Duration::days(7)),
                at(now)
            ),
            false,
        ),
    ];

    println!("\n| Request | First (ms) | Best of 3 (ms) | Bytes |");
    println!("| --- | ---: | ---: | ---: |");
    for (name, path, htmx) in requests {
        let mut times = vec![];
        let mut size = 0;
        for _ in 0..3 {
            let mut request = Request::get(&path);
            if htmx {
                request = request.header("HX-Request", "true");
            }
            let started = Instant::now();
            let (status, body) = test_app.send(request.body(Body::empty()).unwrap()).await;
            times.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(status, StatusCode::OK, "{path}");
            size = body.len();
        }
        let best = times.iter().copied().fold(f64::INFINITY, f64::min);
        println!("| {name} | {:.0} | {best:.0} | {size} |", times[0]);
    }
}
