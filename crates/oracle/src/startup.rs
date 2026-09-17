//! Process composition: application state, the router, task supervision,
//! and shutdown ordering.
//!
//! Shutdown order: stop readiness, drain HTTP, stop background producers
//! (cache warming and ETL), then stop the database writer so every accepted
//! write commits before SQLite closes. Every step is bounded by the
//! configured shutdown timeout.

use crate::{
    Configuration,
    database::Database,
    file_access::{FileAccess, FileData, S3FileAccess},
    oracle::Oracle,
    routes::{
        add_event_entries, create_event, daily_observations, dashboard_handler, download,
        event_detail_handler, event_stats_handler, events_cards_handler, events_handler,
        events_rows_handler, files, forecast_handler, forecasts, get_event, get_event_entry,
        get_npub, get_pubkey, get_stations, healthy, list_events, observations,
        oracle_info_handler, raw_data_handler, ready, update_data, upload, warm_forecast_cache,
        weather_handler,
    },
    weather_data::{WeatherAccess, WeatherData},
};
use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{
        Method, StatusCode,
        header::{self, ACCEPT, CONTENT_TYPE},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use log::{error, info, warn};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::Semaphore,
    task::{JoinError, JoinHandle},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower_http::cors::{Any, CorsLayer};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

const MAX_UPLOAD_BYTES: usize = 30 * 1024 * 1024;
/// Source data arrives hourly, so a 30 minute refresh keeps the cache at
/// most 30 minutes stale.
const FORECAST_CACHE_REFRESH: Duration = Duration::from_secs(30 * 60);

type TaskResult = Result<Result<()>, JoinError>;

/// Background producers of database writes. The runtime stops them after
/// HTTP drains and before the writer closes.
#[derive(Clone, Default)]
pub struct Background {
    tasks: TaskTracker,
    stopping: CancellationToken,
}

impl Background {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.is_cancelled()
    }

    /// Rejects new work, then waits for running tasks.
    pub async fn stop(&self) {
        self.stopping.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}

/// Capabilities handlers receive. Handlers never see database connections.
pub struct AppState {
    pub static_dir: PathBuf,
    pub remote_url: String,
    pub file_access: Arc<dyn FileData>,
    pub weather_db: Arc<dyn WeatherData>,
    pub oracle: Arc<Oracle>,
    pub database: Database,
    forecast_cache: Mutex<HashMap<String, String>>,
    background: Background,
    etl_slot: Arc<Semaphore>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EtlRejected {
    AlreadyRunning,
    ShuttingDown,
}

impl AppState {
    pub fn new(
        remote_url: String,
        static_dir: PathBuf,
        file_access: Arc<dyn FileData>,
        weather_db: Arc<dyn WeatherData>,
        oracle: Arc<Oracle>,
        database: Database,
        background: Background,
    ) -> Self {
        Self {
            static_dir,
            remote_url,
            file_access,
            weather_db,
            oracle,
            database,
            forecast_cache: Mutex::new(HashMap::new()),
            background,
            etl_slot: Arc::new(Semaphore::new(1)),
        }
    }

    pub fn cached_forecast(&self, station_id: &str) -> Option<String> {
        self.forecast_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(station_id)
            .cloned()
    }

    pub fn cache_forecast(&self, station_id: String, html: String) {
        self.forecast_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(station_id, html);
    }

    pub fn clear_forecast_cache(&self) {
        self.forecast_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Runs one ETL pass in the background. Only one pass runs at a time and
    /// none start once shutdown has begun.
    pub fn start_etl(self: &Arc<Self>) -> Result<u64, EtlRejected> {
        if self.background.is_stopping() {
            return Err(EtlRejected::ShuttingDown);
        }
        let permit = self
            .etl_slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| EtlRejected::AlreadyRunning)?;
        let etl_process_id: u64 = rand::random();
        let state = self.clone();
        self.background.tasks.spawn(async move {
            let _permit = permit;
            info!("starting etl process: {}", etl_process_id);
            match state.oracle.etl_data(etl_process_id).await {
                Ok(()) => info!("completed etl process: {}", etl_process_id),
                Err(e) => error!("failed etl process: {} {}", etl_process_id, e),
            }
        });
        Ok(etl_process_id)
    }

    /// Waits until the running ETL pass, if any, has finished.
    pub async fn wait_for_etl(&self) {
        let _permit = self.etl_slot.acquire().await;
    }
}

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::routes::events::get_npub,
        crate::routes::events::get_pubkey,
        crate::routes::events::list_events,
        crate::routes::events::create_event,
        crate::routes::events::get_event,
        crate::routes::events::add_event_entries,
        crate::routes::events::get_event_entry,
        crate::routes::events::update_data,
        crate::routes::stations::forecasts,
        crate::routes::stations::observations,
        crate::routes::stations::daily_observations,
        crate::routes::stations::get_stations,
        crate::routes::files::download::download,
        crate::routes::files::get_names::files,
        crate::routes::files::upload::upload,
    ),
    components(
        schemas(
                crate::routes::files::get_names::Files,
                crate::oracle::Error,
                crate::events::Event,
                crate::events::WeatherEntry,
                crate::events::AddEventEntry,
                crate::events::CreateEvent,
                crate::routes::events::Pubkey,
                crate::routes::events::Base64Pubkey
            )
    ),
    tags(
        (name = "noaa data oracle api", description = "a RESTful api that acts as an oracle for NOAA forecast and observation data")
    )
)]
struct ApiDoc;

/// Opens every dependency and returns the shared state with its writer.
/// The writer must be running before the oracle validates its key.
async fn build_app_state(
    configuration: &Configuration,
    database: Database,
    background: Background,
) -> Result<Arc<AppState>> {
    let file_access: Arc<dyn FileData> = match &configuration.s3_bucket {
        Some(bucket) => {
            info!("Using S3 bucket '{}' for file access", bucket);
            Arc::new(S3FileAccess::new(bucket.clone(), configuration.s3_endpoint.clone()).await)
        }
        None => Arc::new(FileAccess::new(
            configuration.weather_dir.to_string_lossy().into_owned(),
        )),
    };
    // Weather queries read local parquet files directly with DuckDB.
    let local_file_access: Arc<dyn FileData> = Arc::new(FileAccess::new(
        configuration.weather_dir.to_string_lossy().into_owned(),
    ));
    let weather_db = Arc::new(WeatherAccess::new(local_file_access));
    let oracle = Oracle::new(
        database.clone(),
        weather_db.clone(),
        &configuration.private_key,
    )
    .await
    .map_err(|e| anyhow!("error setting up oracle: {}", e))?;
    Ok(Arc::new(AppState::new(
        configuration.remote_url.clone(),
        configuration.static_dir.clone(),
        file_access,
        weather_db,
        Arc::new(oracle),
        database,
        background,
    )))
}

pub fn app(app_state: Arc<AppState>) -> Router {
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
        // Probes
        .route("/health", get(ready))
        .route("/ready", get(ready))
        .route("/healthy", get(healthy))
        // API routes
        .route("/files", get(files))
        .route("/file/{file_name}", get(download))
        .route("/file/{file_name}", post(upload))
        .route("/stations", get(get_stations))
        .route("/stations/forecasts", get(forecasts))
        .route("/stations/observations", get(observations))
        .route("/stations/daily-observations", get(daily_observations))
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
        .with_state(app_state)
        .layer(middleware::from_fn(log_request))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .merge(Scalar::with_url("/docs", api_docs))
        .layer(cors)
}

async fn log_request(request: Request<Body>, next: Next) -> impl IntoResponse {
    let started = std::time::Instant::now();
    let method = request.method().clone();
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    info!(target: "http_request", "new request, {} {}", method, path);

    let response = next.run(request).await;
    info!(
        target: "http_response",
        "response, code: {}, time: {:?}",
        response.status().as_str(),
        started.elapsed()
    );

    response
}

/// Serves static files with explicit MIME type mappings.
/// This avoids relying on the system's MIME database which may be missing in containers.
async fn serve_static_file(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Response {
    // Only plain file names below the static directory are served.
    let relative = std::path::Path::new(&path);
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let file_path = state.static_dir.join(relative);

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
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") | Some("htm") => "text/html; charset=utf-8",
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

/// Runs the oracle until a termination signal arrives or a supervised task
/// stops unexpectedly, then shuts down in order.
pub async fn run_until_stop(configuration: Configuration) -> Result<()> {
    // Register before initialization so a TERM received during startup persists.
    let requested = CancellationToken::new();
    let signal_task = install_termination_signal(requested.clone())?;
    let result = async {
        if let Some(runtime) = ApplicationRuntime::build(&configuration, requested).await? {
            runtime.run_until_stop().await?;
        }
        Ok(())
    }
    .await;
    signal_task.abort();
    let _ = signal_task.await;
    result
}

/// Owns every supervised task. Dropping it aborts them so nothing outlives
/// the runtime unsupervised.
struct ApplicationRuntime {
    database: Database,
    background: Background,
    requested: CancellationToken,
    http_shutdown: CancellationToken,
    database_shutdown: CancellationToken,
    shutdown_timeout: Duration,
    http: Option<JoinHandle<Result<()>>>,
    writer: Option<JoinHandle<Result<()>>>,
}

impl ApplicationRuntime {
    async fn build(
        configuration: &Configuration,
        requested: CancellationToken,
    ) -> Result<Option<Self>> {
        if requested.is_cancelled() {
            return Ok(None);
        }
        let (database, writer) = Database::open(&configuration.event_dir)
            .await
            .context("initialize SQLite")?;
        let database_shutdown = CancellationToken::new();
        let mut runtime = Self {
            database: database.clone(),
            background: Background::new(),
            requested,
            http_shutdown: CancellationToken::new(),
            database_shutdown: database_shutdown.clone(),
            shutdown_timeout: configuration.shutdown_timeout,
            http: None,
            writer: Some(tokio::spawn(writer.run(database_shutdown))),
        };
        let background = runtime.background.clone();
        let prepare = async {
            tokio::fs::create_dir_all(&configuration.weather_dir)
                .await
                .with_context(|| {
                    format!(
                        "create weather directory {}",
                        configuration.weather_dir.display()
                    )
                })?;
            let state = build_app_state(configuration, database, background).await?;
            let listener = TcpListener::bind(configuration.listen)
                .await
                .with_context(|| format!("bind HTTP listener {}", configuration.listen))?;
            let address = listener
                .local_addr()
                .context("read HTTP listener address")?;
            Ok::<_, anyhow::Error>((state, listener, address))
        };
        let prepared = tokio::select! {
            biased;
            () = runtime.requested.cancelled() => Ok(None),
            completion = wait_for_task(&mut runtime.writer) => {
                Err(task_failure("database writer", completion, true)
                    .unwrap_or_else(|| anyhow!("database writer stopped during startup")))
            }
            result = prepare => result.map(Some),
        };
        let (state, listener, address) = match prepared {
            Ok(Some(prepared)) if !runtime.requested.is_cancelled() => prepared,
            result => {
                // Startup failures and signals must still close SQLite cleanly.
                let cleanup = runtime.shutdown().await;
                return first_failure(result.map(|_| ()), cleanup).map(|()| None);
            }
        };
        spawn_cache_warmer(&state);
        runtime.http = Some(spawn_http(
            listener,
            app(state),
            runtime.http_shutdown.clone(),
        ));
        info!("NOAA Oracle listening on http://{address}");
        info!("  Docs:         http://{address}/docs");
        info!("  Weather data: {}", configuration.weather_dir.display());
        info!("  Event DB:     {}", configuration.event_dir.display());
        info!("  Static:       {}", configuration.static_dir.display());
        Ok(Some(runtime))
    }

    async fn run_until_stop(mut self) -> Result<()> {
        let failure = tokio::select! {
            biased;
            () = self.requested.cancelled() => {
                info!("shutdown requested");
                None
            }
            completion = wait_for_task(&mut self.http) => {
                task_failure("HTTP", completion, true)
            }
            completion = wait_for_task(&mut self.writer) => {
                task_failure("database writer", completion, true)
            }
        };
        first_failure(failure.map_or(Ok(()), Err), self.shutdown().await)
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.database.stop_readiness();
        self.http_shutdown.cancel();
        let timeout = self.shutdown_timeout;
        match tokio::time::timeout(timeout, self.drain()).await {
            Ok(result) => result,
            Err(_) => {
                self.database_shutdown.cancel();
                self.abort_tasks();
                if self.http.is_some() {
                    let _ = wait_for_task(&mut self.http).await;
                }
                if self.writer.is_some() {
                    let _ = wait_for_task(&mut self.writer).await;
                }
                Err(anyhow!(
                    "shutdown exceeded {}s; accepted write outcomes may be unknown",
                    timeout.as_secs_f64()
                ))
            }
        }
    }

    async fn drain(&mut self) -> Result<()> {
        let http_failure = if self.http.is_some() {
            task_failure("HTTP", wait_for_task(&mut self.http).await, false)
        } else {
            None
        };
        // Handlers and background tasks may submit writes until they finish.
        // Only then stop the writer and wait for its connection to close.
        self.background.stop().await;
        self.database_shutdown.cancel();
        let writer_failure = if self.writer.is_some() {
            task_failure(
                "database writer",
                wait_for_task(&mut self.writer).await,
                false,
            )
        } else {
            None
        };
        first_failure(
            http_failure.map_or(Ok(()), Err),
            writer_failure.map_or(Ok(()), Err),
        )
    }

    fn abort_tasks(&self) {
        for task in [&self.http, &self.writer].into_iter().flatten() {
            task.abort();
        }
    }
}

impl Drop for ApplicationRuntime {
    fn drop(&mut self) {
        // Dropping a JoinHandle detaches it; never leave an unsupervised task.
        self.abort_tasks();
    }
}

fn spawn_cache_warmer(state: &Arc<AppState>) {
    let state = state.clone();
    let stopping = state.background.stopping.clone();
    state.background.tasks.clone().spawn(async move {
        warm_forecast_cache(&state).await;
        let mut interval = tokio::time::interval(FORECAST_CACHE_REFRESH);
        interval.tick().await; // the first tick completes immediately
        loop {
            tokio::select! {
                biased;
                () = stopping.cancelled() => break,
                _ = interval.tick() => {
                    state.clear_forecast_cache();
                    warm_forecast_cache(&state).await;
                }
            }
        }
    });
}

fn spawn_http(
    listener: TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("serve HTTP")
    })
}

async fn wait_for_task(task: &mut Option<JoinHandle<Result<()>>>) -> TaskResult {
    let result = match task.as_mut() {
        Some(task) => task.await,
        None => std::future::pending().await,
    };
    task.take();
    result
}

fn task_failure(name: &str, result: TaskResult, unexpected: bool) -> Option<anyhow::Error> {
    match result {
        Ok(Ok(())) if unexpected => Some(anyhow!("{name} task stopped unexpectedly")),
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.context(format!("{name} task failed"))),
        Err(error) => Some(anyhow!(error).context(format!("{name} task panicked or was aborted"))),
    }
}

fn first_failure(first: Result<()>, second: Result<()>) -> Result<()> {
    match (first, second) {
        (Err(error), Err(additional)) => {
            warn!("additional shutdown failure: {additional:#}");
            Err(error)
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(unix)]
fn install_termination_signal(requested: CancellationToken) -> Result<JoinHandle<()>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(tokio::spawn(async move {
        tokio::select! { _ = terminate.recv() => {}, _ = interrupt.recv() => {} }
        requested.cancel();
    }))
}

#[cfg(not(unix))]
fn install_termination_signal(requested: CancellationToken) -> Result<JoinHandle<()>> {
    Ok(tokio::spawn(async move {
        if let Err(error) = tokio::signal::ctrl_c().await {
            error!("termination signal failed: {error}");
        }
        requested.cancel();
    }))
}

#[cfg(test)]
mod lifecycle_tests;
