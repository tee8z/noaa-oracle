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

/// Weather data whose first preparation pass waits until `finish_first` is
/// notified, then fails if `first_fails`; later passes succeed at once.
/// Counts forecast queries.
struct HeldPreparation {
    finish_first: Notify,
    first_fails: bool,
    passes: AtomicUsize,
    forecast_queries: AtomicUsize,
}

impl HeldPreparation {
    fn new(first_fails: bool) -> Arc<Self> {
        Arc::new(Self {
            finish_first: Notify::new(),
            first_fails,
            passes: AtomicUsize::new(0),
            forecast_queries: AtomicUsize::new(0),
        })
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
    async fn prepare_files(&self, _: &CancellationToken) -> Result<usize, weather_data::Error> {
        if self.passes.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(0);
        }
        self.finish_first.notified().await;
        if self.first_fails {
            Err(weather_data::Error::Io(std::io::Error::other("disk full")))
        } else {
            Ok(1)
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

    weather.finish_first.notify_one();
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

    weather.finish_first.notify_one();
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
