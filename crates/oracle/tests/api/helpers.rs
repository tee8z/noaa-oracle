use async_trait::async_trait;
use axum::Router;
use log::{info, LevelFilter};
use mockall::mock;
use nostr_sdk::{
    hashes::sha256::Hash as Sha256Hash,
    nips::nip98::{HttpData, HttpMethod},
    Event, EventBuilder, Keys, Url,
};
use oracle::{
    app, create_folder, oracle::Oracle, setup_logger, AppState, Database, FileData, WeatherData,
};
use rand::Rng;
use std::{
    str::FromStr,
    sync::{Arc, Once},
};

pub struct TestApp {
    pub app: Router,
    pub oracle: Arc<Oracle>,
}
static INIT_LOGGER: Once = Once::new();
fn init_logger() {
    INIT_LOGGER.call_once(|| {
        setup_logger().level(LevelFilter::Debug).apply().unwrap();
    });
}

pub fn random_test_number() -> i32 {
    let mut rng = rand::thread_rng();
    rng.gen_range(10000..99999)
}

pub async fn spawn_app(weather_db: Arc<dyn WeatherData>) -> TestApp {
    init_logger();
    create_folder("./test_data");
    let random_test_number = random_test_number();
    info!("test number: {}", random_test_number);
    let test_folder = format!("./test_data/{}", random_test_number);
    create_folder(&test_folder.clone());
    let event_data = format!("{}/event_data", test_folder);
    create_folder(&event_data.clone());

    let db = Arc::new(Database::new(&event_data).await.unwrap());
    let private_key_file_path = String::from("./oracle_private_key.pem");
    let oracle = Arc::new(
        Oracle::new(db, weather_db.clone(), &private_key_file_path)
            .await
            .unwrap(),
    );

    let app_state = AppState {
        ui_dir: String::from("./ui"),
        remote_url: String::from("http://127.0.0.1:9100"),
        weather_db,
        file_access: Arc::new(MockFileAccess::new()),
        oracle: oracle.clone(),
    };
    let app = app(app_state);

    TestApp { app, oracle }
}

mock! {
    pub FileAccess {}
    #[async_trait]
    impl FileData for FileAccess {
        async fn grab_file_names(&self, params: oracle::FileParams) -> Result<Vec<String>, oracle::Error>;
        fn current_folder(&self) -> String;
        fn build_file_paths(&self, file_names: Vec<String>) -> Vec<String>;
        fn build_file_path(&self, filename: &str, file_generated_at: time::OffsetDateTime) -> String;
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
        async fn stations(&self) -> Result<Vec<oracle::Station>, oracle::weather_data::Error>;
    }
}

pub async fn create_auth_event(
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

    EventBuilder::http_auth(http_data)
        .sign_with_keys(keys)
        .expect("Failed to sign event")
}
