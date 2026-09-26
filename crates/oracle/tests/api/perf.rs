//! Page, fragment and API timings on a copy of real weather data. Not run
//! by default; point `ORACLE_PERF_DATA` at a weather directory whose newest
//! files are from the last day or so:
//!
//! ```text
//! ORACLE_PERF_DATA=~/weather cargo test -p oracle --test api perf -- --ignored --nocapture
//! ```
//!
//! Copies and folds are made first, as the running oracle makes them. Each
//! request then runs three times; the first is uncached (the station list,
//! forecast fragments and DuckDB's Parquet metadata start empty). Pages run
//! both without the time zone cookie (UTC days) and with a New York reader's.

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

pub(super) const AIRPORTS: &str = "KATL,KLAX,KORD,KDFW,KDEN,KJFK,KSFO,KSEA,KLAS,KMCO,KEWR,KMIA,KPHX,KIAH,\
        KBOS,KMSP,KFLL,KDTW,KPHL,KLGA,KBWI,KSLC,KDCA,KSAN,KTPA,KPDX,KSTL,KHNL,KBNA,KAUS,KMCI,\
        KRDU,KMKE,KSMF,KCLT,KPIT,KSAT,KOAK,KCLE,KSJC,KIND,KCVG,KCMH,KJAN,KRSW,KABQ,KANC,KOMA,\
        KBUF,KPBI,KBDL,KPVD,KBTV,KPWM,KMHT,KBOI,KBIL,KFSD,KFAR,KGEG,KICT,KLIT,KLEX,KBHM,KMEM,\
        KJAX,KCHS,KRIC,KORF,KCRW,KPNS,KMOB,KSHV,KMSY,KTUL,KELP,KTUS,KCOS,KGRR,KDSM,KMSN,KDLH,\
        KBZN,KGJT,KRAP,KFCA,KCYS,KJAR,KSGF,KFSM";

const STATIONS: &str = "KPWM,KBTV,KBED";

/// A reader in New York.
const NEW_YORK: &str = "tz=America/New_York";

/// The target for every page, fragment and the API calls pages depend on.
const BUDGET_MS: f64 = 400.0;

fn at(time: OffsetDateTime) -> String {
    time.to_offset(UtcOffset::UTC).format(&Rfc3339).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ORACLE_PERF_DATA"]
async fn page_timings_on_real_data() {
    let directory =
        std::env::var("ORACLE_PERF_DATA").expect("ORACLE_PERF_DATA must name a weather directory");
    let weather = Arc::new(WeatherAccess::with_derived_forecasts(
        Arc::new(FileAccess::new(directory.clone())),
        &std::path::Path::new(&directory).join("derived"),
    ));
    let preparing = Instant::now();
    let copied = weather
        .prepare_files(&tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    println!(
        "copies made: {copied}; copies and folds ready in {:.1}s",
        preparing.elapsed().as_secs_f64()
    );
    let test_app = spawn_app(weather).await;

    let now = OffsetDateTime::now_utc();
    let event = event_at(now);
    assert!(test_app.create_event(&event).await.0.is_success());
    let today = now.replace_time(Time::MIDNIGHT);
    // A finished 24-hour competition window, and one that started at 02:00
    // today and is still running.
    let finished = today - Duration::hours(21);
    let running = today + Duration::hours(2);

    // (name, path, htmx, cookie, counts against the budget)
    let requests: Vec<(&str, String, bool, Option<&str>, bool)> = vec![
        ("station list", "/stations".into(), false, None, true),
        ("events page", "/events".into(), false, None, true),
        (
            "event detail page",
            format!("/events/{}", event.id),
            false,
            None,
            true,
        ),
        ("raw data page", "/raw".into(), false, None, true),
        ("dashboard page, UTC", "/".into(), false, None, true),
        (
            "dashboard page, New York",
            "/".into(),
            false,
            Some(NEW_YORK),
            true,
        ),
        (
            "dashboard content, New York",
            "/".into(),
            true,
            Some(NEW_YORK),
            true,
        ),
        (
            "weather map, UTC",
            "/fragments/weather?view=map".into(),
            true,
            None,
            true,
        ),
        (
            "weather map, New York",
            "/fragments/weather?view=map".into(),
            true,
            Some(NEW_YORK),
            true,
        ),
        (
            "weather list, UTC",
            "/fragments/weather?view=list".into(),
            true,
            None,
            true,
        ),
        (
            "weather list, New York",
            "/fragments/weather?view=list".into(),
            true,
            Some(NEW_YORK),
            true,
        ),
        (
            "station search, New York",
            "/fragments/weather?view=list&q=denver".into(),
            true,
            Some(NEW_YORK),
            true,
        ),
        (
            "station popup KORD, UTC",
            "/fragments/station/KORD".into(),
            true,
            None,
            true,
        ),
        (
            "station popup KORD, New York",
            "/fragments/station/KORD".into(),
            true,
            Some(NEW_YORK),
            true,
        ),
        (
            "forecast KSAW, New York",
            "/fragments/forecast/KSAW".into(),
            true,
            Some(NEW_YORK),
            true,
        ),
        (
            "API forecasts, entry form (3 stations, next 2 days)",
            format!(
                "/stations/forecasts?station_ids={STATIONS}&start={}&end={}",
                at(now),
                at(now + Duration::days(2))
            ),
            false,
            None,
            true,
        ),
        (
            "API forecasts, leaderboard (3 stations, finished window)",
            format!(
                "/stations/forecasts?station_ids={STATIONS}&start={}&end={}",
                at(finished),
                at(finished + Duration::days(1))
            ),
            false,
            None,
            true,
        ),
        (
            "API forecasts, scoring baseline (3 stations, issues before the window)",
            format!(
                "/stations/forecasts?station_ids={STATIONS}&start={}&end={}&generated_start={}&generated_end={}",
                at(finished),
                at(finished + Duration::days(1)),
                at(finished - Duration::days(7)),
                at(finished - Duration::nanoseconds(1))
            ),
            false,
            None,
            true,
        ),
        (
            "API forecasts, admin page (90 airports, next 2 days)",
            format!(
                "/stations/forecasts?station_ids={AIRPORTS}&start={}&end={}",
                at(now),
                at(now + Duration::days(2))
            ),
            false,
            None,
            true,
        ),
        (
            "API observations, live progress (window start to now)",
            format!(
                "/stations/observations?station_ids={STATIONS}&start={}&end={}",
                at(running),
                at(now)
            ),
            false,
            None,
            true,
        ),
        (
            "API daily observations (3 stations, 7 days)",
            format!(
                "/stations/daily-observations?station_ids={STATIONS}&start={}&end={}",
                at(today - Duration::days(7)),
                at(now)
            ),
            false,
            None,
            true,
        ),
    ];

    println!("\n| Request | First (ms) | Best of 3 (ms) | Bytes sent |");
    println!("| --- | ---: | ---: | ---: |");
    let mut over = vec![];
    for (name, path, htmx, cookie, budgeted) in requests {
        let mut times = vec![];
        let mut size = 0;
        for _ in 0..3 {
            // As a browser asks: compressed, if the server compresses.
            let mut request = Request::get(&path).header("Accept-Encoding", "gzip");
            if htmx {
                request = request.header("HX-Request", "true");
            }
            if let Some(cookie) = cookie {
                request = request.header("Cookie", cookie);
            }
            let started = Instant::now();
            let (status, body) = test_app.send(request.body(Body::empty()).unwrap()).await;
            times.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(status, StatusCode::OK, "{path}");
            size = body.len();
        }
        let best = times.iter().copied().fold(f64::INFINITY, f64::min);
        println!("| {name} | {:.0} | {best:.0} | {size} |", times[0]);
        if budgeted && times[0] > BUDGET_MS {
            over.push(format!("{name}: {:.0} ms", times[0]));
        }
    }
    assert!(over.is_empty(), "over {BUDGET_MS} ms: {over:?}");
}

/// Where the weather fragment's time goes: each query it makes, alone,
/// for UTC and New York days.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ORACLE_PERF_DATA"]
async fn weather_fragment_breakdown() {
    use oracle::{ForecastRequest, ObservationRequest, TemperatureUnit, calendar::Calendar};
    let directory =
        std::env::var("ORACLE_PERF_DATA").expect("ORACLE_PERF_DATA must name a weather directory");
    let weather = Arc::new(WeatherAccess::with_derived_forecasts(
        Arc::new(FileAccess::new(directory.clone())),
        &std::path::Path::new(&directory).join("derived"),
    ));
    weather
        .prepare_files(&tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let ids: Vec<String> = AIRPORTS.split(',').map(str::to_string).collect();
    let now = OffsetDateTime::now_utc();
    for calendar in [
        Calendar::Utc,
        Calendar::from_zone_name("America/New_York").unwrap(),
    ] {
        let day_start = calendar.start_of_day(now);
        let day_end = calendar.start_of(calendar.date_of(now).next_day().unwrap());
        let today = ObservationRequest {
            start: Some(day_start),
            end: Some(now),
            station_ids: AIRPORTS.into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let recent = ObservationRequest {
            start: Some(now - Duration::hours(24)),
            ..today.clone()
        };
        let forecast = ForecastRequest {
            start: Some(day_start),
            end: Some(day_end),
            generated_start: Some(day_start - Duration::days(1)),
            generated_end: Some(day_start - Duration::nanoseconds(1)),
            station_ids: AIRPORTS.into(),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        for round in 0..3 {
            let started = Instant::now();
            weather.observation_data(&today, ids.clone()).await.unwrap();
            let a = started.elapsed().as_secs_f64() * 1000.0;
            let started = Instant::now();
            weather
                .observation_data(&recent, ids.clone())
                .await
                .unwrap();
            let b = started.elapsed().as_secs_f64() * 1000.0;
            let started = Instant::now();
            weather
                .calendar_forecasts(&forecast, ids.clone(), calendar)
                .await
                .unwrap();
            let c = started.elapsed().as_secs_f64() * 1000.0;
            let started = Instant::now();
            let stations = weather.stations().await.unwrap();
            let d = started.elapsed().as_secs_f64() * 1000.0;
            let started = Instant::now();
            let (x, y, z) = tokio::join!(
                weather.observation_data(&today, ids.clone()),
                weather.observation_data(&recent, ids.clone()),
                weather.calendar_forecasts(&forecast, ids.clone(), calendar),
            );
            let e = started.elapsed().as_secs_f64() * 1000.0;
            let _ = (x.unwrap(), y.unwrap(), z.unwrap());
            println!(
                "{} round {round}: today obs {a:.0} ms, 24 h obs {b:.0} ms, forecasts {c:.0} ms, \
                 stations {d:.0} ms ({}), all three at once {e:.0} ms",
                calendar.name(),
                stations.len()
            );
        }
    }
}
