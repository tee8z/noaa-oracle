use crate::helpers::spawn_app;
use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use oracle::{
    Forecast, ForecastRequest, Observation, ObservationRequest, Station,
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

#[tokio::test]
async fn stale_views_serve_immediately_while_all_refreshes_wait_for_admission() {
    let weather = Arc::new(CountedWeather::default());
    let app = spawn_app(weather.clone()).await;
    oracle::routes::warm_caches(&app.state).await;
    let first = app.state.heavy().turn().await.unwrap();
    let second = app.state.heavy().turn().await.unwrap();
    let queries = weather.queries.load(Ordering::SeqCst);
    weather.peak.store(0, Ordering::SeqCst);
    app.state.new_data();
    for path in ["/fragments/forecast/KORD", "/fragments/weather"] {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(weather.queries.load(Ordering::SeqCst), queries);
    drop((first, second));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while weather.queries.load(Ordering::SeqCst) < queries + 7
            || weather.active.load(Ordering::SeqCst) > 0
            || app.state.heavy().free_turns() < 2
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(weather.peak.load(Ordering::SeqCst) <= 2);
}
