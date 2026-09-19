use super::*;
use crate::{events::NewEvent, signing::SigningKey};
use axum::routing::post;
use dlctix::EventLockingConditions;
use std::{
    future::Future,
    io::{Read, Write},
    path::Path,
    sync::Arc,
};
use tokio::sync::Notify;
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

#[tokio::test]
async fn etl_runs_one_at_a_time_and_not_during_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, database) = runtime(directory.path()).await;
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
    let files: Arc<dyn FileData> = Arc::new(NoFiles);
    let weather: Arc<dyn WeatherData> = Arc::new(WeatherAccess::new(files.clone()));
    let oracle = Oracle::new(
        database.clone(),
        Sources::new(Arc::new(NoaaWeather::new(weather.clone())), []),
        &directory.path().join("oracle.pem"),
        system_clock(),
    )
    .await
    .unwrap();
    let state = Arc::new(AppState::new(AppParts {
        remote_url: "http://localhost".into(),
        static_dir: directory.path().to_path_buf(),
        weather_dir: directory.path().to_path_buf(),
        auth: AuthPolicy::new("http://localhost", [], []),
        file_access: files,
        weather_db: weather,
        oracle: Arc::new(oracle),
        database,
        background: runtime.background.clone(),
    }));
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
