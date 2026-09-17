//! Shared fixtures: a temporary SQLite database with its writer, a fresh
//! signing key, a router, and mocks for the file and weather layers.

use async_trait::async_trait;
use axum::Router;
use log::LevelFilter;
use mockall::mock;
use nostr::{
    event::{Event, FinalizeEvent, IntoEventBuilder},
    key::Keys,
    nips::nip98::{HttpData, HttpMethod, Sha256Hash},
    types::Url,
};
use oracle::{
    AppState, Background, Database, FileData, WeatherData, app, oracle::Oracle, setup_logger,
};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Once},
};
use tokio_util::sync::CancellationToken;

pub struct TestApp {
    pub app: Router,
    pub oracle: Arc<Oracle>,
    pub state: Arc<AppState>,
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
    init_logger();
    let directory = tempfile::tempdir().expect("temporary directory");
    let (database, writer) = Database::open(&directory.path().join("event_data"))
        .await
        .expect("open database");
    let shutdown = CancellationToken::new();
    tokio::spawn(writer.run(shutdown.clone()));
    let private_key_file_path = directory.path().join("oracle_private_key.pem");
    let oracle = Arc::new(
        Oracle::new(database.clone(), weather_db.clone(), &private_key_file_path)
            .await
            .expect("oracle"),
    );
    let state = Arc::new(AppState::new(
        String::from("http://127.0.0.1:9100"),
        PathBuf::from("./static"),
        Arc::new(MockFileAccess::new()),
        weather_db,
        oracle.clone(),
        database,
        Background::new(),
    ));
    let app = app(state.clone());

    TestApp {
        app,
        oracle,
        state,
        _writer: WriterGuard(shutdown),
        _directory: directory,
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

/// NIP-98 payload tag: the SHA-256 of the request body.
pub fn payload_hash(body: &[u8]) -> Sha256Hash {
    Sha256Hash::from_byte_array(Sha256::digest(body).into())
}

/// Signs a NIP-98 event the way the coordinator's client does.
pub fn create_auth_event(
    method: &str,
    url: &str,
    payload_hash: Option<Sha256Hash>,
    keys: &Keys,
) -> Event {
    let http_method = HttpMethod::from_str(method).unwrap();
    let http_url = Url::from_str(url).unwrap();
    let mut http_data = HttpData::new(http_url, http_method);

    if let Some(hash) = payload_hash {
        http_data = http_data.payload(hash);
    }

    http_data
        .into_event_builder()
        .finalize(keys)
        .expect("Failed to sign event")
}
