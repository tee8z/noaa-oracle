//! Process composition: application state, the router, task supervision,
//! and shutdown ordering.
//!
//! Shutdown order: stop readiness, drain HTTP, stop background producers
//! (cache warming and ETL), then stop the database writer so every accepted
//! write commits before SQLite closes. Every step is bounded by the
//! configured shutdown timeout.

use crate::{
    auth::AuthPolicy,
    config::{Configuration, Storage},
    database::Database,
    file_access::{FileAccess, FileData, S3FileAccess},
    oracle::{Oracle, system_clock},
    routes::{
        add_event_entries, create_event, daily_observations, dashboard_handler, download,
        event_detail_handler, event_stats_handler, events_handler, files, forecast_handler,
        forecasts, get_event, get_event_entry, get_npub, get_pubkey, get_stations, healthy,
        list_events, list_sources, observations, oracle_info_handler, raw_data_handler, ready,
        station_handler, ui::policy::content_security_policy, update_data, upload,
        warm_forecast_cache, weather_handler,
    },
    sources::{NoaaWeather, Sources},
    templates::assets::serve_asset,
    weather_data::{self, Station, WeatherAccess, WeatherData},
};
use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Request},
    handler::Handler,
    http::{
        Method,
        header::{ACCEPT, CONTENT_TYPE},
    },
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
};
use log::{error, info, warn};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
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

/// Parquet uploads from the daemon.
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
/// Every other request body: event and entry JSON is a few KiB.
const MAX_BODY_BYTES: usize = 256 * 1024;
/// Source data arrives hourly, so a 30 minute refresh keeps the cache at
/// most 30 minutes stale.
const FORECAST_CACHE_REFRESH: Duration = Duration::from_secs(30 * 60);
/// Forecast fragments kept in memory. Only known stations are cached, so
/// this bounds memory even if the station list grows unexpectedly.
const MAX_CACHED_FORECASTS: usize = 4_096;
/// How often derived forecast files are checked for, besides after each
/// upload: catches files added by another oracle process.
const PREPARE_FILES_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Leave room in the deployment's ten minute readiness window for opening
/// dependencies. A larger or slower archive continues preparing in the
/// background, with exact queries falling back to published files.
const STARTUP_WEATHER_BUDGET: Duration = Duration::from_secs(9 * 60);

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
    pub remote_url: String,
    /// Local directory uploads land in and DuckDB reads.
    pub weather_dir: PathBuf,
    pub auth: AuthPolicy,
    pub file_access: Arc<dyn FileData>,
    pub weather_db: Arc<dyn WeatherData>,
    pub oracle: Arc<Oracle>,
    pub database: Database,
    forecast_cache: Mutex<HashMap<String, String>>,
    stations: Arc<StationList>,
    background: Background,
    etl_slot: Arc<Semaphore>,
    /// This process, as a lease holder. Unique per start.
    instance: String,
    /// Wakes the task that prepares new data files for queries.
    files_added: tokio::sync::Notify,
}

/// The station list and when it was read. It only changes when new files
/// add a station, and reading it scans a month of observation files, so
/// pages get the list last read while a refresh runs in the background.
#[derive(Default)]
struct StationList {
    current: tokio::sync::Mutex<Option<(std::time::Instant, Arc<Vec<Station>>)>>,
    /// New files arrived since the list was read.
    stale: AtomicBool,
    refreshing: AtomicBool,
}

/// Processing scores entries and signs attestations. During a blue/green
/// deploy two oracles share one database, and only this lease's holder runs it.
const ETL_LEASE: &str = "etl";
/// Longer than the processing interval, so the holder keeps the lease between
/// passes; a stopped holder's passes move to another process after this.
const ETL_LEASE_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, PartialEq, Eq)]
pub enum EtlRejected {
    AlreadyRunning,
    ShuttingDown,
}

/// Everything [`AppState`] is built from.
pub struct AppParts {
    pub remote_url: String,
    pub weather_dir: PathBuf,
    pub auth: AuthPolicy,
    pub file_access: Arc<dyn FileData>,
    pub weather_db: Arc<dyn WeatherData>,
    pub oracle: Arc<Oracle>,
    pub database: Database,
    pub background: Background,
}

impl AppState {
    pub fn new(parts: AppParts) -> Self {
        let AppParts {
            remote_url,
            weather_dir,
            auth,
            file_access,
            weather_db,
            oracle,
            database,
            background,
        } = parts;
        Self {
            remote_url,
            weather_dir,
            auth,
            file_access,
            weather_db,
            oracle,
            database,
            forecast_cache: Mutex::new(HashMap::new()),
            stations: Arc::default(),
            background,
            etl_slot: Arc::new(Semaphore::new(1)),
            instance: format!("oracle-{}", uuid::Uuid::now_v7()),
            files_added: tokio::sync::Notify::new(),
        }
    }

    /// A data file was published: prepare it for queries.
    pub fn file_added(&self) {
        self.files_added.notify_one();
    }

    pub fn cached_forecast(&self, station_id: &str) -> Option<String> {
        self.forecast_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(station_id)
            .cloned()
    }

    /// Caches a rendered forecast. Callers pass only known station ids.
    pub fn cache_forecast(&self, station_id: String, html: String) {
        let mut cache = self
            .forecast_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if cache.len() < MAX_CACHED_FORECASTS || cache.contains_key(&station_id) {
            cache.insert(station_id, html);
        }
    }

    /// Drops cached forecasts and marks the station list for a refresh after
    /// new data arrives.
    pub fn clear_forecast_cache(&self) {
        self.forecast_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.stations.stale.store(true, Ordering::Release);
    }

    /// Every station in the observation files. Only the first call waits
    /// for the files to be read; later calls get the list last read, and
    /// start a background refresh when new files arrived or the list is
    /// older than [`FORECAST_CACHE_REFRESH`].
    pub async fn stations(&self) -> Result<Arc<Vec<Station>>, weather_data::Error> {
        let mut current = self.stations.current.lock().await;
        if let Some((read_at, stations)) = current.as_ref() {
            if read_at.elapsed() >= FORECAST_CACHE_REFRESH
                || self.stations.stale.load(Ordering::Acquire)
            {
                self.refresh_stations();
            }
            return Ok(stations.clone());
        }
        self.stations.stale.store(false, Ordering::Release);
        let stations = Arc::new(self.weather_db.stations().await?);
        *current = Some((std::time::Instant::now(), stations.clone()));
        Ok(stations)
    }

    /// Rereads the station list in the background, once at a time.
    fn refresh_stations(&self) {
        if self.stations.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let list = self.stations.clone();
        let weather_db = self.weather_db.clone();
        self.background.tasks.spawn(async move {
            list.stale.store(false, Ordering::Release);
            match weather_db.stations().await {
                Ok(stations) => {
                    *list.current.lock().await =
                        Some((std::time::Instant::now(), Arc::new(stations)));
                }
                Err(error) => {
                    list.stale.store(true, Ordering::Release);
                    warn!("cannot refresh the station list: {error}");
                }
            }
            list.refreshing.store(false, Ordering::Release);
        });
    }

    /// Whether `station_id` appears in the observation files.
    pub async fn is_known_station(&self, station_id: &str) -> bool {
        self.stations().await.is_ok_and(|stations| {
            stations
                .iter()
                .any(|station| station.station_id == station_id)
        })
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
            match state
                .database
                .take_lease(ETL_LEASE, &state.instance, ETL_LEASE_TTL)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    info!("another oracle process runs processing; skipped {etl_process_id}");
                    return;
                }
                Err(e) => {
                    warn!("cannot take the processing lease; skipped {etl_process_id}: {e}");
                    return;
                }
            }
            info!("starting etl process: {}", etl_process_id);
            // Keep the lease for as long as the pass runs.
            let renewal = async {
                loop {
                    tokio::time::sleep(ETL_LEASE_TTL / 3).await;
                    match state
                        .database
                        .take_lease(ETL_LEASE, &state.instance, ETL_LEASE_TTL)
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => warn!("another oracle process took the processing lease"),
                        Err(e) => warn!("cannot renew the processing lease: {e}"),
                    }
                }
            };
            let result = tokio::select! {
                result = state.oracle.etl_data(etl_process_id) => result,
                () = renewal => unreachable!("lease renewal never ends"),
            };
            match result {
                Ok(0) => info!("completed etl process: {etl_process_id}"),
                Ok(failed) => warn!("etl process {etl_process_id}: {failed} events failed"),
                Err(e) => error!("failed etl process: {etl_process_id} {e:#}"),
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
        crate::routes::events::list_sources,
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
                crate::routes::events::ErrorBody,
                crate::routes::events::SourceInfo,
                crate::scoring::Pick,
                crate::sources::Reading,
                crate::events::Event,
                crate::events::EventSummary,
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
    let file_access: Arc<dyn FileData> = match &configuration.storage {
        Storage::S3 { bucket, endpoint } => {
            info!("Using S3 bucket '{}' for file access", bucket);
            Arc::new(S3FileAccess::new(bucket.clone(), endpoint.clone()).await)
        }
        Storage::Local => Arc::new(FileAccess::new(
            configuration.weather_dir.to_string_lossy().into_owned(),
        )),
    };
    // Weather queries read local parquet files directly with DuckDB.
    let local_file_access: Arc<dyn FileData> = Arc::new(FileAccess::new(
        configuration.weather_dir.to_string_lossy().into_owned(),
    ));
    let weather_db: Arc<dyn WeatherData> = Arc::new(WeatherAccess::with_derived_forecasts(
        local_file_access,
        &configuration.weather_dir.join("derived"),
    ));
    let sources = Sources::new(Arc::new(NoaaWeather::new(weather_db.clone())), []);
    let oracle = Oracle::new(
        database.clone(),
        sources,
        &configuration.private_key,
        system_clock(),
    )
    .await
    .context("set up oracle")?;
    info!("oracle npub: {}", oracle.npub());
    if configuration.coordinators.is_empty() {
        warn!("no coordinator_pubkeys configured: nobody can create events");
    }
    if configuration.uploaders.is_empty() {
        warn!("no uploader_pubkeys configured: nobody can upload data");
    }
    Ok(Arc::new(AppState::new(AppParts {
        remote_url: configuration.remote_url.clone(),
        weather_dir: configuration.weather_dir.clone(),
        auth: AuthPolicy::new(
            &configuration.remote_url,
            configuration.coordinators.clone(),
            configuration.uploaders.clone(),
        ),
        file_access,
        weather_db,
        oracle: Arc::new(oracle),
        database,
        background,
    })))
}

pub fn app(app_state: Arc<AppState>) -> Router {
    let api_docs = ApiDoc::openapi();
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([ACCEPT, CONTENT_TYPE])
        .allow_origin(Any);

    // Pages and the fragments htmx swaps into them.
    let ui = Router::new()
        .route("/", get(dashboard_handler))
        .route("/events", get(events_handler))
        .route("/events/{event_id}", get(event_detail_handler))
        .route("/raw", get(raw_data_handler))
        .route("/fragments/oracle-info", get(oracle_info_handler))
        .route("/fragments/event-stats", get(event_stats_handler))
        .route("/fragments/weather", get(weather_handler))
        .route("/fragments/forecast/{station_id}", get(forecast_handler))
        .route("/fragments/station/{station_id}", get(station_handler))
        .layer(middleware::from_fn(content_security_policy));

    Router::new()
        .merge(ui)
        .route("/assets/{file}", get(serve_asset))
        // Probes
        .route("/health", get(ready))
        .route("/ready", get(ready))
        .route("/healthy", get(healthy))
        // API routes
        .route("/files", get(files))
        .route(
            "/file/{file_name}",
            get(download).post(upload.layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))),
        )
        .route("/stations", get(get_stations))
        .route("/stations/forecasts", get(forecasts))
        .route("/stations/observations", get(observations))
        .route("/stations/daily-observations", get(daily_observations))
        .route("/oracle/npub", get(get_npub))
        .route("/oracle/pubkey", get(get_pubkey))
        .route("/oracle/sources", get(list_sources))
        .route("/oracle/update", post(update_data))
        .route("/oracle/events", get(list_events))
        .route("/oracle/events", post(create_event))
        .route("/oracle/events/{event_id}", get(get_event))
        .route("/oracle/events/{event_id}/entries", post(add_event_entries))
        .route(
            "/oracle/events/{event_id}/entries/{entry_id}",
            get(get_event_entry),
        )
        .with_state(app_state)
        .layer(middleware::from_fn(log_request))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
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
    let request_logging = started.elapsed();
    let handling = std::time::Instant::now();
    let response = next.run(request).await;
    let handled = handling.elapsed();
    let logging = std::time::Instant::now();
    info!(
        target: "http_response",
        "response, code: {}, time: {:?}",
        response.status().as_str(),
        started.elapsed()
    );

    let response_logging = logging.elapsed();
    if started.elapsed().as_millis() >= 350 {
        info!(target: "http_timing", "slow HTTP phases: request_log={:.1}ms handler={:.1}ms response_log={:.1}ms",
            request_logging.as_secs_f64() * 1000.0, handled.as_secs_f64() * 1000.0, response_logging.as_secs_f64() * 1000.0);
    }
    response
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
            prepare_weather_before_serving(&state, STARTUP_WEATHER_BUDGET).await;
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
        spawn_etl_schedule(&state, configuration.etl_interval);
        spawn_lease_release(&state);
        runtime.http = Some(spawn_http(
            listener,
            app(state),
            runtime.http_shutdown.clone(),
        ));
        info!("NOAA Oracle listening on http://{address}");
        info!("  Docs:         http://{address}/docs");
        info!("  Weather data: {}", configuration.weather_dir.display());
        info!("  Event DB:     {}", configuration.event_dir.display());
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
        // Prepared files make uncached station details fast. Let the first
        // visitors use the query slots before periodic fragment warming starts.
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

/// Makes derived copies of new forecast files: at start, after each
/// upload, and every [`PREPARE_FILES_INTERVAL`]. Stops between files on
/// shutdown.
fn spawn_file_preparation(state: &Arc<AppState>) -> tokio::sync::oneshot::Receiver<()> {
    let state = state.clone();
    let stopping = state.background.stopping.clone();
    let (prepared, ready) = tokio::sync::oneshot::channel();
    state.background.tasks.clone().spawn(async move {
        let mut prepared = Some(prepared);
        loop {
            if let Err(error) = state.weather_db.prepare_files(&stopping).await {
                warn!("cannot prepare weather files for queries: {error}");
            }
            if let Some(prepared) = prepared.take() {
                let _ = prepared.send(());
            }
            tokio::select! {
                biased;
                () = stopping.cancelled() => break,
                () = state.files_added.notified() => {}
                () = tokio::time::sleep(PREPARE_FILES_INTERVAL) => {}
            }
        }
    });
    ready
}

/// Prepare only the active forecast window and preload station metadata before
/// accepting visitors. The tracked worker survives the deadline, so timing out
/// does not start a second compaction or abandon an in-flight file write.
async fn prepare_weather_before_serving(state: &Arc<AppState>, budget: Duration) {
    let started = std::time::Instant::now();
    let prepared = spawn_file_preparation(state);
    let preparation = async {
        if let Err(error) = state.stations().await {
            warn!("cannot preload the station list: {error}");
        }
        if prepared.await.is_err() {
            warn!("weather preparation stopped before its first pass completed");
        }
    };
    if tokio::time::timeout(budget, preparation).await.is_err() {
        warn!(
            "weather startup preparation exceeded {}s; continuing in the background with published-file fallback",
            budget.as_secs_f64()
        );
    }
    info!(
        "weather startup preparation took {:.3}s",
        started.elapsed().as_secs_f64()
    );
}

/// On shutdown, hands processing over to another oracle process at once. The
/// lease is released only after a running pass finishes, so two passes never
/// overlap.
fn spawn_lease_release(state: &Arc<AppState>) {
    let state = state.clone();
    let stopping = state.background.stopping.clone();
    state.background.tasks.clone().spawn(async move {
        stopping.cancelled().await;
        let _idle = state.etl_slot.acquire().await;
        if let Err(e) = state
            .database
            .release_lease(ETL_LEASE, &state.instance)
            .await
        {
            warn!("cannot release the processing lease: {e}");
        }
    });
}

/// Starts a processing pass every `interval` so events are attested once
/// their signing date passes, even when no new data arrives.
fn spawn_etl_schedule(state: &Arc<AppState>, interval: Duration) {
    let state = state.clone();
    let stopping = state.background.stopping.clone();
    state.background.tasks.clone().spawn(async move {
        let mut interval = tokio::time::interval(interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = stopping.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(rejected) = state.start_etl() {
                        info!("scheduled processing skipped: {rejected:?}");
                    }
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
