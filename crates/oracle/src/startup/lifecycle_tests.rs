use super::*;
use crate::{events::NewEvent, signing::SigningKey};
use axum::http::StatusCode;
use axum::routing::post;
use dlctix::EventLockingConditions;
use std::{
    future::Future,
    io::{Read, Write},
    path::Path,
    sync::{Arc, atomic::AtomicUsize},
};
use tokio::sync::Notify;
use tower::ServiceExt;
use uuid::Uuid;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("lifecycle test timed out")
}

async fn runtime(directory: &Path) -> (ApplicationRuntime, Database) {
    let (database, writer) = bounded(Database::open(&directory.join("event_data")))
        .await
        .unwrap();
    let database_shutdown = CancellationToken::new();
    let runtime = ApplicationRuntime {
        database: database.clone(),
        background: Background::new(),
        requested: CancellationToken::new(),
        http_shutdown: CancellationToken::new(),
        database_shutdown: database_shutdown.clone(),
        shutdown_timeout: Duration::from_secs(3),
        http: None,
        metrics: None,
        metrics_address: None,
        writer: Some(tokio::spawn(writer.run(database_shutdown))),
    };
    (runtime, database)
}

fn event_data() -> NewEvent {
    let directory = tempfile::tempdir().unwrap();
    let key = SigningKey::load_or_create(&directory.path().join("oracle.pem")).unwrap();
    let id = Uuid::now_v7();
    let start = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    NewEvent {
        id,
        source: "noaa_weather".into(),
        signing_date: start + time::Duration::hours(3),
        start_observation_date: start,
        end_observation_date: start + time::Duration::hours(1),
        locations: vec!["KORD".into()],
        metrics: vec!["temp_high".into()],
        number_of_values_per_entry: 1,
        total_allowed_entries: 2,
        number_of_places_win: 1,
        nonce: key.new_event_nonce(id),
        event_announcement: EventLockingConditions {
            locking_points: vec![],
            expiry: Some(1),
        },
        coordinator_pubkey: "npub1coordinator".into(),
        unlisted: false,
        scoring_rules: crate::scoring::ScoringRules::Fixed,
        lines: vec![],
    }
}

async fn serve(runtime: &mut ApplicationRuntime, router: Router) -> SocketAddr {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    runtime.http = Some(spawn_http(listener, router, runtime.http_shutdown.clone()));
    address
}

fn post_event(address: SocketAddr) -> String {
    let mut stream = std::net::TcpStream::connect_timeout(&address, TEST_TIMEOUT).unwrap();
    stream.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    stream
        .write_all(
            b"POST /event HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

async fn event_count(directory: &Path) -> i64 {
    let (database, writer) = Database::open(&directory.join("event_data")).await.unwrap();
    let count = i64::try_from(database.list_events(&[], 100).await.unwrap().len()).unwrap();
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    writer.run(shutdown).await.unwrap();
    count
}

#[tokio::test]
async fn shutdown_finishes_an_accepted_http_request_before_stopping_the_writer() {
    let directory = tempfile::tempdir().unwrap();
    let (mut runtime, database) = runtime(directory.path()).await;
    let requested = runtime.requested.clone();
    let http_shutdown = runtime.http_shutdown.clone();
    let writer_shutdown = runtime.database_shutdown.clone();
    let admitted = Arc::new(Notify::new());
    let finish_request = Arc::new(Notify::new());
    let handler_database = database.clone();
    let handler_admitted = admitted.clone();
    let handler_finish = finish_request.clone();
    let router = Router::new().route(
        "/event",
        post(move || {
            let database = handler_database.clone();
            let admitted = handler_admitted.clone();
            let finish = handler_finish.clone();
            async move {
                admitted.notify_one();
                finish.notified().await;
                let event = event_data();
                database.add_event(&event).await.unwrap();
                event.id.to_string()
            }
        }),
    );
    let address = serve(&mut runtime, router).await;
    let running = tokio::spawn(runtime.run_until_stop());
    let client = tokio::task::spawn_blocking(move || post_event(address));
    bounded(admitted.notified()).await;
    assert!(database.is_ready().await);

    requested.cancel();
    bounded(http_shutdown.cancelled()).await;
    assert!(
        !database.is_ready().await,
        "readiness stops before draining"
    );
    assert!(!writer_shutdown.is_cancelled(), "writer outlives handlers");
    assert!(!running.is_finished());

    finish_request.notify_one();
    let response = bounded(client).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    bounded(running).await.unwrap().unwrap();
    assert!(writer_shutdown.is_cancelled());
    assert!(!database.is_writer_available());
    assert_eq!(bounded(event_count(directory.path())).await, 1);
}

#[tokio::test]
async fn background_work_drains_before_the_writer_closes() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
    let requested = runtime.requested.clone();
    let writer_shutdown = runtime.database_shutdown.clone();
    let stopping = runtime.background.stopping.clone();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let task_started = started.clone();
    let task_release = release.clone();
    let task_database = database.clone();
    runtime.background.tasks.spawn(async move {
        task_started.notify_one();
        task_release.notified().await;
        task_database.add_event(&event_data()).await.unwrap();
    });
    let running = tokio::spawn(runtime.run_until_stop());
    bounded(started.notified()).await;

    requested.cancel();
    bounded(stopping.cancelled()).await;
    assert!(
        !writer_shutdown.is_cancelled(),
        "writer waits for producers"
    );
    assert!(!running.is_finished());

    release.notify_one();
    bounded(running).await.unwrap().unwrap();
    assert!(writer_shutdown.is_cancelled());
    assert_eq!(bounded(event_count(directory.path())).await, 1);
}

#[tokio::test]
async fn an_unexpected_writer_exit_stops_http() {
    let directory = tempfile::tempdir().unwrap();
    let (mut runtime, database) = runtime(directory.path()).await;
    let http_shutdown = runtime.http_shutdown.clone();
    let router = Router::new().route("/event", post(|| async { "unused" }));
    let address = serve(&mut runtime, router).await;
    runtime.writer.as_ref().unwrap().abort();

    let error = bounded(runtime.run_until_stop()).await.unwrap_err();
    assert!(error.to_string().contains("database writer"), "{error:#}");
    assert!(http_shutdown.is_cancelled());
    assert!(!database.is_ready().await);
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn an_unexpected_http_exit_drains_and_closes_the_writer() {
    let directory = tempfile::tempdir().unwrap();
    let (mut runtime, database) = runtime(directory.path()).await;
    let writer_shutdown = runtime.database_shutdown.clone();
    let router = Router::new().route("/event", post(|| async { "unused" }));
    serve(&mut runtime, router).await;
    runtime.http.as_ref().unwrap().abort();

    let error = bounded(runtime.run_until_stop()).await.unwrap_err();
    assert!(error.to_string().contains("HTTP"), "{error:#}");
    assert!(writer_shutdown.is_cancelled());
    assert!(!database.is_writer_available());
    assert!(
        database.list_events(&[], 100).await.is_err(),
        "read pool must be closed"
    );
}

/// No weather files at all.
struct NoFiles;

#[async_trait::async_trait]
impl FileData for NoFiles {
    async fn grab_file_names(
        &self,
        _: crate::file_access::FileParams,
    ) -> Result<Vec<String>, crate::file_access::Error> {
        Ok(vec![])
    }
    fn build_file_paths(&self, _: Vec<String>) -> Vec<String> {
        vec![]
    }
    fn build_file_path(&self, file: &crate::file_access::ParquetFileName) -> String {
        file.to_string()
    }
    async fn download_file(
        &self,
        file: &crate::file_access::ParquetFileName,
    ) -> Result<Body, crate::file_access::Error> {
        Err(crate::file_access::Error::NotFound(file.to_string()))
    }
}

async fn app_state(
    directory: &Path,
    runtime: &ApplicationRuntime,
    database: Database,
    weather: Arc<dyn WeatherData>,
) -> Arc<AppState> {
    let files: Arc<dyn FileData> = Arc::new(NoFiles);
    let oracle = Oracle::new(
        database.clone(),
        Sources::new(Arc::new(NoaaWeather::new(weather.clone())), []),
        &directory.join("oracle.pem"),
        system_clock(),
    )
    .await
    .unwrap();
    Arc::new(AppState::new(AppParts {
        remote_url: "http://localhost".into(),
        weather_dir: directory.to_path_buf(),
        auth: AuthPolicy::new("http://localhost", [], []),
        file_access: files,
        weather_db: weather,
        oracle: Arc::new(oracle),
        database,
        background: runtime.background.clone(),
    }))
}

#[tokio::test]
async fn etl_runs_one_at_a_time_and_not_during_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
    let weather: Arc<dyn WeatherData> = Arc::new(WeatherAccess::new(Arc::new(NoFiles)));
    let state = app_state(directory.path(), &runtime, database, weather).await;
    let first = state.start_etl().unwrap();
    let second = state.start_etl();
    assert!(matches!(second, Ok(_) | Err(EtlRejected::AlreadyRunning)));
    bounded(state.wait_for_etl()).await;
    assert_ne!(first, 0, "etl ids are random");
    let requested = runtime.requested.clone();
    requested.cancel();
    bounded(runtime.run_until_stop()).await.unwrap();
    assert_eq!(state.start_etl(), Err(EtlRejected::ShuttingDown));
}

/// Weather data whose first preparation pass copies `files` files, each on
/// the turns it is given and once `release` hands it a permit, then fails if
/// `first_fails`; later passes succeed at once. Counts copies and forecast
/// queries.
struct HeldPreparation {
    release: Semaphore,
    files: usize,
    first_fails: bool,
    passes: AtomicUsize,
    copied: AtomicUsize,
    forecast_queries: AtomicUsize,
}

impl HeldPreparation {
    fn new(first_fails: bool) -> Arc<Self> {
        Self::copying(1, first_fails)
    }

    fn copying(files: usize, first_fails: bool) -> Arc<Self> {
        Arc::new(Self {
            release: Semaphore::new(0),
            files,
            first_fails,
            passes: AtomicUsize::new(0),
            copied: AtomicUsize::new(0),
            forecast_queries: AtomicUsize::new(0),
        })
    }

    fn copied(&self) -> usize {
        self.copied.load(Ordering::SeqCst)
    }

    fn forecast_queries(&self) -> usize {
        self.forecast_queries.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl WeatherData for HeldPreparation {
    async fn forecasts_data(
        &self,
        _: &crate::routes::ForecastRequest,
        _: Vec<String>,
    ) -> Result<Vec<weather_data::Forecast>, weather_data::Error> {
        self.forecast_queries.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }
    async fn observation_data(
        &self,
        _: &crate::routes::ObservationRequest,
        _: Vec<String>,
    ) -> Result<Vec<weather_data::Observation>, weather_data::Error> {
        Ok(vec![])
    }
    async fn daily_observations(
        &self,
        _: &crate::routes::ObservationRequest,
        _: Vec<String>,
    ) -> Result<Vec<weather_data::DailyObservation>, weather_data::Error> {
        Ok(vec![])
    }
    async fn stations(&self) -> Result<Vec<Station>, weather_data::Error> {
        Ok(vec![])
    }
    async fn prepare_files(
        &self,
        _: &CancellationToken,
        turns: &weather_data::PreparationTurns,
    ) -> Result<usize, weather_data::Error> {
        if self.passes.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(0);
        }
        for _ in 0..self.files {
            let Some(_turns) = turns().await else {
                return Ok(self.copied());
            };
            self.release.acquire().await.unwrap().forget();
            self.copied.fetch_add(1, Ordering::SeqCst);
        }
        if self.first_fails {
            Err(weather_data::Error::Io(std::io::Error::other("disk full")))
        } else {
            Ok(self.files)
        }
    }
}

async fn status(router: &Router, path: &str) -> StatusCode {
    let request = Request::get(path).body(Body::empty()).unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

async fn wait_until(mut condition: impl AsyncFnMut() -> bool) {
    bounded(async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
}

#[tokio::test]
async fn the_cache_warmer_and_readiness_wait_for_the_first_preparation() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
    let weather = HeldPreparation::new(false);
    let state = app_state(directory.path(), &runtime, database, weather.clone()).await;
    let router = app(state.clone());
    spawn_file_preparation(&state);
    spawn_cache_warmer(&state);

    // The first pass is still running: the process answers, but isn't ready.
    wait_until(async || weather.passes.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(weather.forecast_queries(), 0, "the warmer must wait");
    assert_eq!(
        status(&router, "/ready").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(status(&router, "/health").await, StatusCode::OK);
    assert_eq!(status(&router, "/healthy").await, StatusCode::OK);

    weather.release.add_permits(1);
    wait_until(async || status(&router, "/ready").await == StatusCode::OK).await;
    wait_until(async || weather.forecast_queries() > 0).await;

    // Later passes don't take readiness away.
    state.file_added();
    wait_until(async || weather.passes.load(Ordering::SeqCst) == 2).await;
    assert_eq!(status(&router, "/ready").await, StatusCode::OK);

    runtime.requested.cancel();
    bounded(runtime.run_until_stop()).await.unwrap();
}

#[tokio::test]
async fn a_failed_first_preparation_warms_the_cache_but_is_not_ready() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
    let weather = HeldPreparation::new(true);
    let state = app_state(directory.path(), &runtime, database, weather.clone()).await;
    let router = app(state.clone());
    spawn_file_preparation(&state);
    spawn_cache_warmer(&state);

    weather.release.add_permits(1);
    // Queries still answer from the published files, so the warmer runs.
    wait_until(async || weather.forecast_queries() > 0).await;
    assert_eq!(
        status(&router, "/ready").await,
        StatusCode::SERVICE_UNAVAILABLE
    );

    // The next pass (after an upload, or the interval) succeeds.
    state.file_added();
    wait_until(async || status(&router, "/ready").await == StatusCode::OK).await;

    runtime.requested.cancel();
    bounded(runtime.run_until_stop()).await.unwrap();
}

/// Weather data with one station, KDEN, eligible for any query. Counts how
/// often eligibility was judged and observations were read.
#[derive(Default)]
struct CountedEligibility {
    judged: AtomicUsize,
    observed: AtomicUsize,
}

#[async_trait::async_trait]
impl WeatherData for CountedEligibility {
    async fn forecasts_data(
        &self,
        _: &crate::routes::ForecastRequest,
        _: Vec<String>,
    ) -> Result<Vec<weather_data::Forecast>, weather_data::Error> {
        Ok(vec![])
    }
    async fn observation_data(
        &self,
        _: &crate::routes::ObservationRequest,
        _: Vec<String>,
    ) -> Result<Vec<weather_data::Observation>, weather_data::Error> {
        self.observed.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }
    async fn daily_observations(
        &self,
        _: &crate::routes::ObservationRequest,
        _: Vec<String>,
    ) -> Result<Vec<weather_data::DailyObservation>, weather_data::Error> {
        Ok(vec![])
    }
    async fn stations(&self) -> Result<Vec<Station>, weather_data::Error> {
        Ok(vec![Station {
            station_id: "KDEN".into(),
            station_name: "Denver Intl".into(),
            state: "CO".into(),
            iata_id: "DEN".into(),
            elevation_m: Some(1655.0),
            latitude: 39.86,
            longitude: -104.67,
        }])
    }
    async fn eligible_stations(
        &self,
        days: u32,
        window_hours: u32,
        now: time::OffsetDateTime,
    ) -> Result<Vec<weather_data::Eligibility>, weather_data::Error> {
        self.judged.fetch_add(1, Ordering::SeqCst);
        let eligibility = |station_id: &str, eligible| weather_data::Eligibility {
            station_id: station_id.into(),
            clean_days: days,
            days_checked: days,
            last_report: now - time::Duration::minutes(10),
            forecast_through: Some(now + time::Duration::days(2)),
            coverage_checked_at: now,
            recent_window_hours: window_hours,
            recent_window_clean: eligible,
            max_report_gap_seconds: 3600,
            eligible,
        };
        Ok(vec![eligibility("KDEN", true), eligibility("KSAW", false)])
    }
}

async fn get_json(router: &Router, path: &str) -> (StatusCode, serde_json::Value) {
    let request = Request::get(path).body(Body::empty()).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn eligible_stations_are_judged_once_per_query_and_validated() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
    let weather = Arc::new(CountedEligibility::default());
    let state = app_state(directory.path(), &runtime, database, weather.clone()).await;
    let router = app(state.clone());

    for path in [
        "/stations/eligible?days=0",
        "/stations/eligible?days=4",
        "/stations/eligible?days=32",
        "/stations/eligible?days=thirty",
        "/stations/eligible?window_hours=0",
        "/stations/eligible?window_hours=25",
        "/stations/eligible?window_hours=-1",
        "/stations/eligible?window=12",
    ] {
        let (status, body) = get_json(&router, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("invalid request: "),
            "{path}: {body}"
        );
    }
    assert_eq!(weather.judged.load(Ordering::SeqCst), 0);

    let (status, body) = get_json(&router, "/stations/eligible").await;
    assert_eq!(status, StatusCode::OK);
    let stations = body.as_array().unwrap();
    assert_eq!(stations.len(), 1, "{body}");
    let station = stations[0].as_object().unwrap();
    let mut fields: Vec<&str> = station.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "clean_days",
            "coverage_checked_at",
            "days_checked",
            "forecast_through",
            "iata_id",
            "last_report",
            "latitude",
            "longitude",
            "max_report_gap_seconds",
            "recent_window_hours",
            "state",
            "station_id",
            "station_name"
        ]
    );
    assert_eq!(station["station_id"], "KDEN");
    assert_eq!(station["iata_id"], "DEN");
    assert_eq!(station["days_checked"], 3);
    assert!(station["last_report"].as_str().unwrap().ends_with('Z'));
    // Requests leave the gauge alone; it follows the scheduled judgment.
    assert!(
        state
            .metrics()
            .encode()
            .contains("\noracle_eligible_stations 0\n")
    );

    // The defaults spelled out are the same query, served from the cache;
    // other values are judged on their own.
    let (status, _) = get_json(&router, "/stations/eligible?days=3&window_hours=24").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(weather.judged.load(Ordering::SeqCst), 1);
    // Longer histories read weeks of reports: only the operator listener
    // judges them.
    let (status, body) = get_json(&router, "/stations/eligible?days=7&window_hours=6").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "invalid request: days must be between 1 and 3"
    );
    assert_eq!(weather.judged.load(Ordering::SeqCst), 1);
    let operator = crate::metrics::router(state.clone());
    let (status, body) = get_json(&operator, "/stations/eligible?days=7&window_hours=6").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["days_checked"], 7);
    assert_eq!(weather.judged.load(Ordering::SeqCst), 2);
    get_json(&operator, "/stations/eligible?days=7&window_hours=6").await;
    assert_eq!(weather.judged.load(Ordering::SeqCst), 2);
    // A kept long history is not served on the public listener either.
    let (status, _) = get_json(&router, "/stations/eligible?days=7&window_hours=6").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(state.heavy().free_turns(), heavy::HEAVY_TURNS as usize);

    // A collection refresh warms the bounded default history. Longer histories are
    // refreshed when requested, and the gauge follows the default.
    state.refresh_eligibility().await;
    assert_eq!(weather.judged.load(Ordering::SeqCst), 3);
    assert!(
        state
            .metrics()
            .encode()
            .contains("\noracle_eligible_stations 1\n")
    );
    let (status, body) = get_json(&router, "/stations/eligible").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(weather.judged.load(Ordering::SeqCst), 3);

    get_json(&operator, "/stations/eligible?days=7&window_hours=6").await;
    bounded(async {
        while weather.judged.load(Ordering::SeqCst) < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(weather.judged.load(Ordering::SeqCst), 4);

    runtime.requested.cancel();
    bounded(runtime.run_until_stop()).await.unwrap();
}

/// Weather data whose eligibility judgment announces that it started, then
/// waits until `release` is notified.
#[derive(Default)]
struct HeldEligibility {
    started: Notify,
    release: Notify,
}

#[async_trait::async_trait]
impl WeatherData for HeldEligibility {
    async fn forecasts_data(
        &self,
        _: &crate::routes::ForecastRequest,
        _: Vec<String>,
    ) -> Result<Vec<weather_data::Forecast>, weather_data::Error> {
        Ok(vec![])
    }
    async fn observation_data(
        &self,
        _: &crate::routes::ObservationRequest,
        _: Vec<String>,
    ) -> Result<Vec<weather_data::Observation>, weather_data::Error> {
        Ok(vec![])
    }
    async fn daily_observations(
        &self,
        _: &crate::routes::ObservationRequest,
        _: Vec<String>,
    ) -> Result<Vec<weather_data::DailyObservation>, weather_data::Error> {
        Ok(vec![])
    }
    async fn stations(&self) -> Result<Vec<Station>, weather_data::Error> {
        Ok(vec![])
    }
    async fn eligible_stations(
        &self,
        _: u32,
        _: u32,
        _: time::OffsetDateTime,
    ) -> Result<Vec<weather_data::Eligibility>, weather_data::Error> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(vec![])
    }
}

/// Attestation never runs beside heavy work: a processing pass waits for the
/// heavy work already running, then takes every turn until it is done.
#[tokio::test]
async fn a_processing_pass_waits_for_heavy_work_already_running() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
    let weather = Arc::new(HeldEligibility::default());
    let state = app_state(directory.path(), &runtime, database, weather.clone()).await;
    let router = app(state.clone());

    // A request judges a list nobody has asked for yet, on a heavy turn.
    let request = tokio::spawn({
        let router = router.clone();
        async move { status(&router, "/stations/eligible").await }
    });
    bounded(weather.started.notified()).await;
    assert_eq!(state.heavy().free_turns(), heavy::HEAVY_TURNS as usize - 1);

    // The pass starts and waits for it ...
    state.start_etl().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), state.wait_for_etl())
            .await
            .is_err(),
        "the pass must wait for the heavy work already running"
    );
    // ... runs once it is done, and gives every turn back.
    weather.release.notify_one();
    assert_eq!(bounded(request).await.unwrap(), StatusCode::OK);
    bounded(state.wait_for_etl()).await;
    assert_eq!(state.heavy().free_turns(), heavy::HEAVY_TURNS as usize);

    runtime.requested.cancel();
    bounded(runtime.run_until_stop()).await.unwrap();
}

const COMPLETED_PASSES: &str = "oracle_etl_runs_total{result=\"completed\"}";
const LEASE_HELD: &str = "oracle_etl_lease_held";

/// The value of `series` in the metrics, if it is there.
fn metric(state: &AppState, series: &str) -> Option<i64> {
    state
        .metrics()
        .encode()
        .lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

/// Lets the other tasks run until `condition` holds. Never sleeps, so time
/// may be paused.
async fn until(mut condition: impl FnMut() -> bool) {
    bounded(async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// However long the heavy work already running lasts, a processing pass
/// waits for it: past every warning it holds only the turn it gathered, and
/// shutdown ends the wait with that turn given back and nothing processed.
#[tokio::test]
async fn a_processing_pass_waits_out_long_heavy_work_until_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
    let weather: Arc<dyn WeatherData> = Arc::new(WeatherAccess::new(Arc::new(NoFiles)));
    let state = app_state(directory.path(), &runtime, database, weather).await;
    let running = state.heavy().turn().await.unwrap();

    state.start_etl().unwrap();
    // The pass took the lease and gathered the free turn.
    until(|| state.heavy().free_turns() == 0).await;

    // Nothing but the pass waits on the clock now, so minutes can pass
    // without sleeping.
    tokio::time::pause();
    tokio::time::advance(3 * PASS_WAIT).await;
    let waited = "oracle_pass_admission_wait_seconds{pass=\"processing\"}";
    let three_warnings = i64::try_from(3 * PASS_WAIT.as_secs()).unwrap();
    until(|| metric(&state, waited).is_some_and(|seconds| seconds >= three_warnings)).await;
    assert_eq!(
        metric(&state, COMPLETED_PASSES),
        Some(0),
        "the pass must not run beside heavy work"
    );
    assert_eq!(state.heavy().free_turns(), 0);
    assert_eq!(
        state.etl_slot.available_permits(),
        0,
        "the pass still waits"
    );

    runtime.background.stopping.cancel();
    bounded(state.wait_for_etl()).await;
    assert_eq!(metric(&state, COMPLETED_PASSES), Some(0));
    assert_eq!(
        state.heavy().free_turns(),
        heavy::HEAVY_TURNS as usize - 1,
        "the turn the pass gathered is given back"
    );

    drop(running);
    tokio::time::resume();
    runtime.requested.cancel();
    bounded(runtime.run_until_stop()).await.unwrap();
}

/// File preparation gives its turns back between files, so a processing
/// pass that queued behind it runs once the copy under way ends, not after
/// the last one.
#[tokio::test]
async fn a_processing_pass_runs_between_the_copies_of_file_preparation() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
    let weather = HeldPreparation::copying(3, false);
    let state = app_state(directory.path(), &runtime, database, weather.clone()).await;
    spawn_file_preparation(&state);
    // The first copy runs alone.
    until(|| state.heavy().free_turns() == 0).await;

    state.start_etl().unwrap();
    // The pass took the lease and waits behind the copy.
    until(|| metric(&state, LEASE_HELD) == Some(1)).await;
    assert_eq!(metric(&state, COMPLETED_PASSES), Some(0));

    weather.release.add_permits(1);
    bounded(state.wait_for_etl()).await;
    assert_eq!(metric(&state, COMPLETED_PASSES), Some(1));
    assert_eq!(weather.copied(), 1, "the pass runs before the next copy");
    // Preparation goes on with the next copy, alone again.
    until(|| state.heavy().free_turns() == 0).await;

    weather.release.add_permits(2);
    until(|| state.files_prepared()).await;
    assert_eq!(weather.copied(), 3);
    assert_eq!(state.heavy().free_turns(), heavy::HEAVY_TURNS as usize);

    runtime.requested.cancel();
    bounded(runtime.run_until_stop()).await.unwrap();
}

/// A pass that waited for its turns while its lease passed to another
/// process does not run once it has them: two processes never process at
/// once.
#[tokio::test]
async fn a_pass_whose_lease_passed_to_another_process_while_it_waited_does_not_run() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
    let weather: Arc<dyn WeatherData> = Arc::new(WeatherAccess::new(Arc::new(NoFiles)));
    let state = app_state(directory.path(), &runtime, database.clone(), weather).await;
    let running = state.heavy().patient_every_turn().await.unwrap();

    state.start_etl().unwrap();
    until(|| metric(&state, LEASE_HELD) == Some(1)).await;
    // The lease runs out while the pass waits, and another process takes it.
    database
        .release_lease(ETL_LEASE, &state.instance)
        .await
        .unwrap();
    assert!(
        database
            .take_lease(ETL_LEASE, "oracle-peer", ETL_LEASE_TTL)
            .await
            .unwrap()
    );

    drop(running);
    bounded(state.wait_for_etl()).await;
    assert_eq!(
        metric(&state, COMPLETED_PASSES),
        Some(0),
        "the pass must not run"
    );
    assert_eq!(metric(&state, LEASE_HELD), Some(0));
    assert!(
        !database
            .take_lease(ETL_LEASE, &state.instance, ETL_LEASE_TTL)
            .await
            .unwrap(),
        "the other process keeps the lease"
    );
    assert_eq!(state.heavy().free_turns(), heavy::HEAVY_TURNS as usize);

    runtime.requested.cancel();
    bounded(runtime.run_until_stop()).await.unwrap();
}

#[tokio::test]
async fn fixed_observation_windows_are_kept_until_new_data() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
    let weather = Arc::new(CountedEligibility::default());
    let state = app_state(directory.path(), &runtime, database, weather.clone()).await;
    let router = app(state.clone());
    let fixed = "/stations/observations?station_ids=KDEN,KORD\
                 &start=2030-01-01T00:00:00Z&end=2030-01-02T00:00:00Z";
    let reordered = "/stations/observations?station_ids=KORD,KDEN\
                     &start=2030-01-01T00:00:00Z&end=2030-01-02T00:00:00Z";

    for path in [fixed, reordered, fixed] {
        let (status, _) = get_json(&router, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    assert_eq!(weather.observed.load(Ordering::SeqCst), 1);

    // New data: the kept answer is served while one rebuild runs.
    state.new_data();
    let (status, _) = get_json(&router, fixed).await;
    assert_eq!(status, StatusCode::OK);
    bounded(async {
        while weather.observed.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    get_json(&router, fixed).await;
    assert_eq!(weather.observed.load(Ordering::SeqCst), 2);

    // Windows relative to now are read every time.
    for _ in 0..2 {
        get_json(&router, "/stations/observations?station_ids=KDEN").await;
    }
    assert_eq!(weather.observed.load(Ordering::SeqCst), 4);

    runtime.requested.cancel();
    bounded(runtime.run_until_stop()).await.unwrap();
}

#[tokio::test]
async fn weather_requests_wait_briefly_for_a_turn_and_overload_is_turned_away() {
    let admission = Arc::new(Admission::new(1, 1));
    let first = admission.turn(Duration::ZERO).await.unwrap();
    // A second request waits for the first to finish.
    let second = tokio::spawn({
        let admission = admission.clone();
        async move { admission.turn(Duration::from_secs(4)).await.is_some() }
    });
    bounded(async {
        while admission.waiting.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    // The queue is full, so a third is turned away at once.
    assert!(
        bounded(admission.turn(Duration::from_secs(4)))
            .await
            .is_none()
    );
    drop(first);
    assert!(bounded(second).await.unwrap());
    assert_eq!(admission.waiting.load(Ordering::Acquire), 0);

    // A request that waits too long gives up and leaves the queue.
    let held = admission.turn(Duration::ZERO).await.unwrap();
    assert!(
        bounded(admission.turn(Duration::from_millis(20)))
            .await
            .is_none()
    );
    assert_eq!(admission.waiting.load(Ordering::Acquire), 0);
    drop(held);
}
