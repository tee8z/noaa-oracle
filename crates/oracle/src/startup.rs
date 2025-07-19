use crate::{
    add_event_entries, create_event, db, download, files, forecasts, get_event, get_event_entry,
    get_npub, get_pubkey, get_stations, index_handler, list_events, observations,
    oracle::{self, Oracle},
    routes, update_data, upload,
    weather_data::WeatherAccess,
    EventData, FileAccess, FileData, WeatherData,
};
use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Request},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use hyper::{
    header::{ACCEPT, CONTENT_TYPE},
    Method,
};
use log::info;
use std::sync::Arc;
use tower_http::{
    cors::{Any, CorsLayer},
    services::{ServeDir, ServeFile},
};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

#[derive(Clone)]
pub struct AppState {
    pub ui_dir: String,
    pub remote_url: String,
    pub file_access: Arc<dyn FileData>,
    pub weather_db: Arc<dyn WeatherData>,
    pub oracle: Arc<Oracle>,
}

#[derive(OpenApi)]
#[openapi(
    paths(
        routes::events::oracle_routes::get_npub,
        routes::events::oracle_routes::get_pubkey,
        routes::events::oracle_routes::list_events,
        routes::events::oracle_routes::create_event,
        routes::events::oracle_routes::get_event,
        routes::events::oracle_routes::add_event_entries,
        routes::events::oracle_routes::get_event_entry,
        routes::events::oracle_routes::update_data,
        routes::stations::weather_routes::forecasts,
        routes::stations::weather_routes::observations,
        routes::stations::weather_routes::get_stations,
        routes::files::download::download,
        routes::files::get_names::files,
        routes::files::upload::upload,
    ),
    components(
        schemas(
                routes::files::get_names::Files,
                oracle::Error,
                db::Event,
                db::WeatherEntry,
                db::AddEventEntry,
                db::CreateEvent,
                routes::events::oracle_routes::Pubkey,
                routes::events::oracle_routes::Base64Pubkey
            )
    ),
    tags(
        (name = "noaa data oracle api", description = "a RESTful api that acts as an oracle for NOAA forecast and observation data")
    )
)]
struct ApiDoc;

pub async fn build_app_state(
    remote_url: String,
    ui_dir: String,
    data_dir: String,
    event_dir: String,
    private_key_file_path: String,
) -> Result<AppState, anyhow::Error> {
    let file_access = Arc::new(FileAccess::new(data_dir));
    let weather_db = Arc::new(
        WeatherAccess::new(file_access.clone())
            .map_err(|e| anyhow!("error setting up weather data: {}", e))?,
    );

    let event_db = Arc::new(
        EventData::new(&event_dir).map_err(|e| anyhow!("error setting up event data: {}", e))?,
    );
    let oracle = Arc::new(Oracle::new(event_db, weather_db.clone(), &private_key_file_path).await?);

    Ok(AppState {
        ui_dir,
        remote_url,
        weather_db,
        file_access,
        oracle,
    })
}

pub fn app(app_state: AppState) -> Router {
    let api_docs = ApiDoc::openapi();
    // The ui folder needs to be generated and have this relative path from where the binary is being run
    let serve_dir = ServeDir::new("ui").not_found_service(ServeFile::new(app_state.ui_dir.clone()));
    let cors = CorsLayer::new()
        // allow `GET` and `POST` when accessing the resource
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([ACCEPT, CONTENT_TYPE])
        // allow requests from any origin
        .allow_origin(Any);
    Router::new()
        .route("/files", get(files))
        .route("/file/{file_name}", get(download))
        .route("/file/{file_name}", post(upload))
        .route("/stations", get(get_stations))
        .route("/stations/forecasts", get(forecasts))
        .route("/stations/observations", get(observations))
        .route("/oracle/npub", get(get_npub))
        .route("/oracle/pubkey", get(get_pubkey))
        .route("/oracle/update", post(update_data))
        .route("/oracle/events", get(list_events))
        .route("/oracle/events", post(create_event))
        .route("/oracle/events/{event_id}", get(get_event))
        .route("/oracle/events/{event_id}/entries", post(add_event_entries))
        .route(
            "/oracle/events/{event_id}/entries/{entry_id}",
            get(get_event_entry),
        )
        .layer(middleware::from_fn(log_request))
        .layer(DefaultBodyLimit::max(30 * 1024 * 1024)) // max is in bytes
        .route("/", get(index_handler))
        .with_state(Arc::new(app_state))
        .merge(Scalar::with_url("/docs", api_docs))
        .nest_service("/ui", serve_dir.clone())
        .fallback_service(serve_dir)
        .layer(cors)
}

async fn log_request(request: Request<Body>, next: Next) -> impl IntoResponse {
    let now = time::OffsetDateTime::now_utc();
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or_default();
    info!(target: "http_request","new request, {} {}", request.method().as_str(), path);

    let response = next.run(request).await;
    let response_time = time::OffsetDateTime::now_utc() - now;
    info!(target: "http_response", "response, code: {}, time: {}", response.status().as_str(), response_time);

    response
}
