use crate::helpers::spawn_app;
use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use futures::poll;
use oracle::{
    Forecast, ForecastRequest, Observation, ObservationRequest, Station,
    heavy::HEAVY_TURNS,
    weather_data::{DailyObservation, Error, WeatherData},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;

#[derive(Default)]
struct CountedWeather {
    active: AtomicUsize,
    peak: AtomicUsize,
    queries: AtomicUsize,
    /// Of those queries, reads of the station list.
    station_reads: AtomicUsize,
}
struct Running<'a>(&'a AtomicUsize);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl CountedWeather {
    async fn query<T>(&self) -> Result<Vec<T>, Error> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _running = Running(&self.active);
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.queries.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        Ok(vec![])
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
        self.query().await
    }
    async fn observation_data(
        &self,
        _: &ObservationRequest,
        _: Vec<String>,
    ) -> Result<Vec<Observation>, Error> {
        self.query().await
    }
    async fn daily_observations(
        &self,
        _: &ObservationRequest,
        _: Vec<String>,
    ) -> Result<Vec<DailyObservation>, Error> {
        self.query().await
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

#[tokio::test]
async fn warming_bounds_actual_query_overlap_and_waits_for_heavy_admission() {
    let weather = Arc::new(CountedWeather::default());
    let app = spawn_app(weather.clone()).await;
    let first = app.state.heavy().turn().await.unwrap();
    let second = app.state.heavy().turn().await.unwrap();
    let state = app.state.clone();
    let warming = tokio::spawn(async move { oracle::routes::warm_caches(&state).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(weather.queries.load(Ordering::SeqCst), 0);
    drop((first, second));
    tokio::time::timeout(std::time::Duration::from_secs(10), warming)
        .await
        .unwrap()
        .unwrap();
    assert!(weather.queries.load(Ordering::SeqCst) >= 270);
    assert_eq!(weather.peak.load(Ordering::SeqCst), 1);
    assert_eq!(weather.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_interactive_forecast_cache_miss_retains_parallel_queries() {
    let weather = Arc::new(CountedWeather::default());
    let app = spawn_app(weather.clone()).await;
    let response = app
        .app
        .oneshot(
            Request::get("/fragments/forecast/KORD")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(weather.peak.load(Ordering::SeqCst) >= 2);
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

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(5), future)
        .await
        .expect("test timed out")
}

/// Readers can set off a refresh for every stale value they find, so a
/// refresh never waits for a turn: a pass that asks for every turn after
/// them waits for none of them, and a later reader sets each off again.
#[tokio::test]
async fn stale_values_serve_at_once_and_their_refreshes_never_queue_ahead_of_a_pass() {
    let weather = Arc::new(CountedWeather::default());
    let app = spawn_app(weather.clone()).await;
    for path in KEPT {
        assert_eq!(app.get(path).await.0, StatusCode::OK, "{path}");
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
    // stale, and reading it again sets its refresh off.
    for path in KEPT {
        let queries = weather.view_queries();
        assert_eq!(app.get(path).await.0, StatusCode::OK, "{path}");
        bounded(async {
            while weather.view_queries() == queries || heavy.free_turns() < HEAVY_TURNS as usize {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
}
