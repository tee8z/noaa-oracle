//! Shared fixtures: a temporary SQLite database with its writer, a fresh
//! signing key, allowlisted coordinator and uploader keys, a controllable
//! clock, a router, and mocks for the file and weather layers.

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use log::LevelFilter;
use mockall::mock;
use nostr::{
    event::{Event, FinalizeEvent, IntoEventBuilder},
    key::Keys,
    nips::nip98::{HttpData, HttpMethod, Sha256Hash},
    types::Url,
};
use oracle::{
    AppParts, AppState, Background, CreateEvent, Database, FileData, WeatherData, app,
    auth::AuthPolicy,
    oracle::{Clock, Oracle},
    setup_logger,
    sources::{NoaaWeather, Sources},
};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex, Once},
};
use time::{Duration, OffsetDateTime};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

/// The origin the oracle checks NIP-98 signatures against.
pub const ORIGIN: &str = "http://oracle.test";

/// Time the oracle sees. Starts at the real time; tests move it forward.
#[derive(Clone)]
pub struct TestClock(Arc<Mutex<OffsetDateTime>>);

impl TestClock {
    pub fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }

    pub fn set(&self, time: OffsetDateTime) {
        *self.0.lock().unwrap() = time;
    }
}

pub struct TestApp {
    pub app: Router,
    pub oracle: Arc<Oracle>,
    pub state: Arc<AppState>,
    pub clock: TestClock,
    pub coordinator: Keys,
    /// Allowlisted too, but did not create the test's events.
    pub other_coordinator: Keys,
    pub uploader: Keys,
    pub weather_dir: PathBuf,
    _writer: WriterGuard,
    _directory: tempfile::TempDir,
}

/// Stops the writer when the test ends so the temporary directory can go.
struct WriterGuard(CancellationToken);

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

static INIT_LOGGER: Once = Once::new();
fn init_logger() {
    INIT_LOGGER.call_once(|| {
        let _ = setup_logger().level(LevelFilter::Debug).apply();
    });
}

pub async fn spawn_app(weather_db: Arc<dyn WeatherData>) -> TestApp {
    spawn_app_at(weather_db, ORIGIN).await
}

/// Like [`spawn_app`], but signatures are checked against `origin`; for
/// tests that serve the router on a real port.
pub async fn spawn_app_at(weather_db: Arc<dyn WeatherData>, origin: &str) -> TestApp {
    init_logger();
    let directory = tempfile::tempdir().expect("temporary directory");
    let (database, writer) = Database::open(&directory.path().join("event_data"))
        .await
        .expect("open database");
    let shutdown = CancellationToken::new();
    tokio::spawn(writer.run(shutdown.clone()));
    let clock = TestClock(Arc::new(Mutex::new(OffsetDateTime::now_utc())));
    let oracle_clock: Clock = {
        let clock = clock.clone();
        Arc::new(move || clock.now())
    };
    let oracle = Arc::new(
        Oracle::new(
            database.clone(),
            Sources::new(Arc::new(NoaaWeather::new(weather_db.clone())), []),
            &directory.path().join("oracle_private_key.pem"),
            oracle_clock,
        )
        .await
        .expect("oracle"),
    );
    let coordinator = Keys::generate();
    let other_coordinator = Keys::generate();
    let uploader = Keys::generate();
    let weather_dir = directory.path().join("weather_data");
    let state = Arc::new(AppState::new(AppParts {
        remote_url: origin.into(),
        weather_dir: weather_dir.clone(),
        auth: AuthPolicy::new(
            origin,
            [coordinator.public_key(), other_coordinator.public_key()],
            [uploader.public_key()],
        ),
        file_access: Arc::new(MockFileAccess::new()),
        weather_db,
        oracle: oracle.clone(),
        database,
        background: Background::new(),
    }));
    TestApp {
        app: app(state.clone()),
        oracle,
        state,
        clock,
        coordinator,
        other_coordinator,
        uploader,
        weather_dir,
        _writer: WriterGuard(shutdown),
        _directory: directory,
    }
}

impl TestApp {
    pub async fn send(&self, request: Request<Body>) -> (StatusCode, Bytes) {
        let response = self.app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        (
            status,
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
    }

    pub async fn get(&self, path: &str) -> (StatusCode, Bytes) {
        self.send(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> T {
        let (status, body) = self.get(path).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    /// Creates `event` as the allowlisted coordinator.
    pub async fn create_event(&self, event: &CreateEvent) -> (StatusCode, Bytes) {
        let body = serde_json::to_vec(event).unwrap();
        self.send(signed(
            Method::POST,
            "/oracle/events",
            body,
            &self.coordinator,
        ))
        .await
    }

    pub async fn submit_entries(
        &self,
        event_id: Uuid,
        body: serde_json::Value,
    ) -> (StatusCode, Bytes) {
        let path = format!("/oracle/events/{event_id}/entries");
        let body = serde_json::to_vec(&body).unwrap();
        self.send(signed(Method::POST, &path, body, &self.coordinator))
            .await
    }

    /// Runs one processing pass and waits for it.
    pub async fn run_etl(&self) {
        self.state.start_etl().expect("etl starts");
        self.state.wait_for_etl().await;
    }
}

/// A request signed the way the coordinator's and daemon's clients sign.
pub fn signed(method: Method, path: &str, body: Vec<u8>, keys: &Keys) -> Request<Body> {
    let payload = (!body.is_empty()).then(|| payload_hash(&body));
    let event = auth_event(
        method.as_str(),
        &format!("{ORIGIN}{path}"),
        payload,
        keys,
        None,
    );
    with_auth(Request::builder().method(method).uri(path), &event)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

pub fn with_auth(
    builder: axum::http::request::Builder,
    event: &Event,
) -> axum::http::request::Builder {
    builder.header(
        header::AUTHORIZATION,
        format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(event).unwrap())
        ),
    )
}

/// NIP-98 payload tag: the SHA-256 of the request body.
pub fn payload_hash(body: &[u8]) -> Sha256Hash {
    Sha256Hash::from_byte_array(Sha256::digest(body).into())
}

/// Signs a NIP-98 event, optionally at a given time.
pub fn auth_event(
    method: &str,
    url: &str,
    payload_hash: Option<Sha256Hash>,
    keys: &Keys,
    created_at: Option<OffsetDateTime>,
) -> Event {
    let mut http_data = HttpData::new(
        Url::from_str(url).unwrap(),
        HttpMethod::from_str(method).unwrap(),
    );
    if let Some(hash) = payload_hash {
        http_data = http_data.payload(hash);
    }
    let mut builder = http_data.into_event_builder();
    if let Some(created_at) = created_at {
        builder = builder.custom_created_at(nostr::types::Timestamp::from(
            u64::try_from(created_at.unix_timestamp()).unwrap(),
        ));
    }
    builder.finalize(keys).expect("sign event")
}

/// An event starting an hour after `now`: 3 entries, 1 winner, two stations.
pub fn event_at(now: OffsetDateTime) -> CreateEvent {
    let start = now + Duration::hours(1);
    CreateEvent {
        id: Uuid::now_v7(),
        source: None,
        signing_date: start + Duration::hours(26),
        start_observation_date: start,
        end_observation_date: start + Duration::hours(24),
        locations: vec!["KORD".into(), "KSAW".into()],
        number_of_values_per_entry: 2,
        total_allowed_entries: 3,
        number_of_places_win: 1,
        scoring_fields: None,
    }
}

mock! {
    pub FileAccess {}
    #[async_trait]
    impl FileData for FileAccess {
        async fn grab_file_names(&self, params: oracle::FileParams) -> Result<Vec<String>, oracle::file_access::Error>;
        fn build_file_paths(&self, file_names: Vec<String>) -> Vec<String>;
        fn build_file_path(&self, file: &oracle::ParquetFileName) -> String;
        async fn download_file(&self, file: &oracle::ParquetFileName) -> Result<axum::body::Body, oracle::file_access::Error>;
    }
}

mock! {
    pub WeatherAccess{}
    #[async_trait]
    impl WeatherData for WeatherAccess {
        async fn forecasts_data(
            &self,
            req: &oracle::ForecastRequest,
            station_ids: Vec<String>,
        ) -> Result<Vec<oracle::Forecast>, oracle::weather_data::Error>;
        async fn observation_data(
            &self,
            req: &oracle::ObservationRequest,
            station_ids: Vec<String>,
        ) -> Result<Vec<oracle::Observation>, oracle::weather_data::Error>;
        async fn daily_observations(
            &self,
            req: &oracle::ObservationRequest,
            station_ids: Vec<String>,
        ) -> Result<Vec<oracle::DailyObservation>, oracle::weather_data::Error>;
        async fn stations(&self) -> Result<Vec<oracle::Station>, oracle::weather_data::Error>;
    }
}
