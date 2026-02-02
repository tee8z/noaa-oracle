use crate::{
    add_event_entries, create_event, dashboard_handler, db, download, event_detail_handler,
    event_stats_handler, events_cards_handler, events_handler, events_rows_handler, files,
    forecast_handler, forecasts, get_event, get_event_entry, get_npub, get_pubkey, get_stations,
    list_events, observations,
    oracle::{self, Oracle},
    oracle_info_handler, raw_data_handler, routes, update_data, upload,
    weather_data::WeatherAccess,
    weather_handler, Database, FileAccess, FileData, WeatherData,
};
use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use hyper::{
    header::{ACCEPT, CONTENT_TYPE},
    Method,
};
use log::info;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

#[derive(Clone)]
pub struct AppState {
    pub static_dir: String,
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
    static_dir: String,
    data_dir: String,
    event_dir: String,
    private_key_file_path: String,
) -> Result<AppState, anyhow::Error> {
    let file_access = Arc::new(FileAccess::new(data_dir));
    let weather_db = Arc::new(
        WeatherAccess::new(file_access.clone())
            .map_err(|e| anyhow!("error setting up weather data: {}", e))?,
    );

    let db = Arc::new(
        Database::new(&event_dir)
            .await
            .map_err(|e| anyhow!("error setting up SQLite database: {}", e))?,
    );
    let oracle = Arc::new(Oracle::new(db, weather_db.clone(), &private_key_file_path).await?);

    Ok(AppState {
        static_dir,
        remote_url,
        weather_db,
        file_access,
        oracle,
    })
}


pub async fn health_check(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match state.oracle.health_check().await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            log::error!("Health check failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
pub fn app(app_state: AppState) -> Router {
    let api_docs = ApiDoc::openapi();
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([ACCEPT, CONTENT_TYPE])
        .allow_origin(Any);

    Router::new()
        // UI routes
        .route("/", get(dashboard_handler))
        .route("/events", get(events_handler))
        .route("/events/{event_id}", get(event_detail_handler))
        .route("/raw", get(raw_data_handler))
        // HTMX fragment routes
        .route("/fragments/oracle-info", get(oracle_info_handler))
        .route("/fragments/event-stats", get(event_stats_handler))
        .route("/fragments/weather", get(weather_handler))
        .route("/fragments/forecast/{station_id}", get(forecast_handler))
        .route("/fragments/events-rows", get(events_rows_handler))
        .route("/fragments/events-cards", get(events_cards_handler))
        // API routes
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
        // Static files with explicit MIME types
        .route("/static/{*path}", get(serve_static_file))
        .with_state(Arc::new(app_state))
        .layer(middleware::from_fn(log_request))
        .layer(DefaultBodyLimit::max(30 * 1024 * 1024))
        .merge(Scalar::with_url("/docs", api_docs))
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

/// Serves static files with explicit MIME type mappings.
/// This avoids relying on the system's MIME database which may be missing in containers.
async fn serve_static_file(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Response {
    // Prevent directory traversal attacks
    if path.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let file_path = std::path::Path::new(&state.static_dir).join(&path);

    let content = match tokio::fs::read(&file_path).await {
        Ok(content) => content,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let content_type = get_mime_type(&path);

    ([(header::CONTENT_TYPE, content_type)], content).into_response()
}

/// Returns the appropriate MIME type for a file based on its extension.
fn get_mime_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("js") => "application/javascript; charset=utf-8",
        Some("mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("htm") => "text/html; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("webp") => "image/webp",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("eot") => "application/vnd.ms-fontobject",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}
