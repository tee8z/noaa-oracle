use crate::helpers::{TestApp, spawn_app};
use async_trait::async_trait;
use axum::http::StatusCode;
use futures::poll;
use oracle::{
    Forecast, ForecastRequest, Observation, ObservationRequest, Station,
    weather_data::{DailyObservation, Error, WeatherData},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

/// How long each fake query takes, on the paused clock.
const QUERY_TIME: Duration = Duration::from_millis(1);
/// How long a reader waits for a forecast detail nobody has built yet before
/// getting an error with a retry (`FORECAST_TIMEOUT` in the forecast route).
const FORECAST_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Default)]
struct CountedWeather {
    active: AtomicUsize,
    peak: AtomicUsize,
    queries: AtomicUsize,
    /// Of those queries, reads of the station list.
    station_reads: AtomicUsize,
    /// Once set, queries that build views never finish.
    stalled: AtomicBool,
}
struct Running<'a>(&'a AtomicUsize);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl CountedWeather {
    /// Counts a query, running until the guard drops.
    fn start(&self) -> Running<'_> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.queries.fetch_add(1, Ordering::SeqCst);
        Running(&self.active)
    }

    async fn query<T>(&self) -> Result<Vec<T>, Error> {
        let _running = self.start();
        tokio::time::sleep(QUERY_TIME).await;
        Ok(vec![])
    }

    async fn view_query<T>(&self) -> Result<Vec<T>, Error> {
        if self.stalled.load(Ordering::SeqCst) {
            let _running = self.start();
            std::future::pending::<()>().await;
        }
        self.query().await
    }

    /// Queries that build forecast details, weather views and observation
    /// aggregates: all but the station list reads.
    fn view_queries(&self) -> usize {
        self.queries.load(Ordering::SeqCst) - self.station_reads.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl WeatherData for CountedWeather {
    async fn forecasts_data(
        &self,
        _: &ForecastRequest,
        _: Vec<String>,
    ) -> Result<Vec<Forecast>, Error> {
        self.view_query().await
    }
    async fn observation_data(
        &self,
        _: &ObservationRequest,
        _: Vec<String>,
    ) -> Result<Vec<Observation>, Error> {
        self.view_query().await
    }
    async fn daily_observations(
        &self,
        _: &ObservationRequest,
        _: Vec<String>,
    ) -> Result<Vec<DailyObservation>, Error> {
        self.view_query().await
    }
    async fn stations(&self) -> Result<Vec<Station>, Error> {
        self.station_reads.fetch_add(1, Ordering::SeqCst);
        self.query::<()>().await?;
        Ok(vec![Station {
            station_id: "KORD".into(),
            station_name: "Chicago O'Hare".into(),
            state: "IL".into(),
            iata_id: "ORD".into(),
            elevation_m: Some(205.0),
            latitude: 41.9742,
            longitude: -87.9073,
        }])
    }
}

/// [`spawn_app`], then pauses time, so queries and waits take time without
/// sleeping. The database opens first, on real time: its pool's timers
/// would fire early on a paused clock. Nothing these tests run touches it.
async fn spawn_paused(weather: Arc<CountedWeather>) -> TestApp {
    let app = spawn_app(weather).await;
    tokio::time::pause();
    app
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("test timed out")
}

/// Warming waits for heavy admission however long every turn is held, then
/// builds one value and runs one query at a time.
#[tokio::test]
async fn warming_bounds_actual_query_overlap_and_waits_for_heavy_admission() {
    let weather = Arc::new(CountedWeather::default());
    let app = spawn_paused(weather.clone()).await;
    let held = bounded(app.state.heavy().patient_every_turn())
        .await
        .unwrap();
    let state = app.state.clone();
    let warming = tokio::spawn(async move { oracle::routes::warm_caches(&state).await });
    tokio::time::advance(Duration::from_secs(60 * 60)).await;
    assert!(!warming.is_finished(), "warming waits for a turn");
    assert_eq!(weather.queries.load(Ordering::SeqCst), 0);
    drop(held);
    bounded(warming).await.unwrap();
    assert!(weather.queries.load(Ordering::SeqCst) >= 270);
    assert_eq!(weather.peak.load(Ordering::SeqCst), 1);
    assert_eq!(weather.active.load(Ordering::SeqCst), 0);
}

/// A reader who opens a forecast detail nobody has built yet does not wait
/// for a heavy turn: it is built at once, its queries side by side.
#[tokio::test]
async fn an_interactive_forecast_cache_miss_answers_in_parallel_while_every_heavy_turn_is_held() {
    let weather = Arc::new(CountedWeather::default());
    let app = spawn_paused(weather.clone()).await;
    let _held = bounded(app.state.heavy().patient_every_turn())
        .await
        .unwrap();
    let (status, _) = bounded(app.get("/fragments/forecast/KORD")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(weather.peak.load(Ordering::SeqCst) >= 2);
    assert_eq!(weather.active.load(Ordering::SeqCst), 0);
}

/// That build has its own deadline: when its queries stall the reader gets
/// 503 once the deadline passes, and the stalled queries are dropped.
#[tokio::test]
async fn an_interactive_forecast_cache_miss_whose_queries_stall_answers_503_at_its_deadline() {
    let weather = Arc::new(CountedWeather::default());
    let app = spawn_paused(weather.clone()).await;
    let _held = bounded(app.state.heavy().patient_every_turn())
        .await
        .unwrap();
    weather.stalled.store(true, Ordering::SeqCst);
    // Unbounded on purpose: the request's own limits end it, and the time
    // it took tells which one did.
    let asked = tokio::time::Instant::now();
    let (status, _) = app.get("/fragments/forecast/KORD").await;
    let waited = asked.elapsed();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        (FORECAST_DEADLINE..FORECAST_DEADLINE + Duration::from_secs(1)).contains(&waited),
        "answered after {waited:?}"
    );
    assert_eq!(weather.active.load(Ordering::SeqCst), 0);
}

/// Paths whose values are kept and served stale while they are rebuilt.
const KEPT: [&str; 5] = [
    "/fragments/forecast/KORD",
    "/fragments/weather",
    "/fragments/weather?stations=KORD",
    "/fragments/weather?stations=KATL",
    "/stations/observations?station_ids=KORD&start=2030-01-01T00:00:00Z&end=2030-01-02T00:00:00Z",
];

/// Readers can set off a refresh for every stale value they find, so a
/// refresh never waits for a turn: a pass that asks for every turn after
/// them waits for none of them, and a later reader sets each off again.
#[tokio::test]
async fn stale_values_serve_at_once_and_their_refreshes_never_queue_ahead_of_a_pass() {
    let weather = Arc::new(CountedWeather::default());
    let app = spawn_paused(weather.clone()).await;
    for path in KEPT {
        assert_eq!(bounded(app.get(path)).await.0, StatusCode::OK, "{path}");
    }
    let heavy = app.state.heavy();
    let held = bounded(heavy.patient_every_turn()).await.unwrap();
    app.state.new_data();
    let before = weather.view_queries();
    for path in KEPT {
        assert_eq!(bounded(app.get(path)).await.0, StatusCode::OK, "{path}");
    }
    // Anything the reads started gets the chance to ask for a turn first.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    let pass = heavy.patient_every_turn();
    tokio::pin!(pass);
    assert!(poll!(pass.as_mut()).is_pending());
    drop(held);
    // Only the station list's single refresh may go before the pass.
    let turns = bounded(&mut pass).await.expect("every turn");
    assert_eq!(
        weather.view_queries(),
        before,
        "a refresh ran before the pass"
    );
    drop(turns);

    // Those refreshes were dropped, not kept waiting: each value is still
    // stale, and reading it again sets its refresh off. The refresh holds
    // its turn until it is done and turns go out in order, so every turn is
    // free again only once it has run.
    for path in KEPT {
        let queries = weather.view_queries();
        assert_eq!(bounded(app.get(path)).await.0, StatusCode::OK, "{path}");
        drop(
            bounded(heavy.patient_every_turn())
                .await
                .expect("every turn"),
        );
        assert!(weather.view_queries() > queries, "{path} was not refreshed");
    }
}
