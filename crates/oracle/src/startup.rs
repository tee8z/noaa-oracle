//! Process composition: application state, the router, task supervision,
//! and shutdown ordering.
//!
//! Shutdown order: stop readiness, drain HTTP, stop background producers
//! (cache warming and ETL), then stop the database writer so every accepted
//! write commits before SQLite closes. Every step is bounded by the
//! configured shutdown timeout.

use crate::{
    auth::AuthPolicy,
    cache::{Cache, Cached},
    config::{Configuration, Storage},
    database::Database,
    file_access::{FileAccess, FileData, S3FileAccess},
    metrics::{self, Metrics},
    oracle::{Oracle, system_clock},
    routes::ui::WeatherKey,
    routes::{
        add_event_entries, create_event, daily_observations, dashboard_handler, download,
        event_detail_handler, events_handler, files, forecast_handler, forecasts, get_event,
        get_event_entry, get_npub, get_pubkey, get_stations, health, healthy, list_events,
        list_sources, observations, raw_data_handler, ready, station_handler,
        ui::policy::content_security_policy, update_data, upload, warm_caches, weather_handler,
    },
    sources::{NoaaWeather, Sources},
    templates::{assets::serve_asset, fragments::WeatherDisplay},
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
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
    task::{JoinError, JoinHandle},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower_http::compression::{
    CompressionLayer, Predicate,
    predicate::{DefaultPredicate, NotForContentType},
};
use tower_http::cors::{Any, CorsLayer};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

/// Parquet uploads from the daemon.
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
/// Every other request body: event and entry JSON is a few KiB.
const MAX_BODY_BYTES: usize = 256 * 1024;
/// Source data arrives hourly, so a 30 minute refresh keeps the cache at
/// most 30 minutes stale, even when another oracle process received the
/// new files.
const FORECAST_CACHE_REFRESH: Duration = Duration::from_secs(30 * 60);
/// Forecast fragments kept in memory, the least recently used dropped
/// first: known stations in the readers' time zones.
const MAX_CACHED_FORECASTS: usize = 4_096;
/// Current weather is rebuilt at least this often: the latest reports
/// arrive hourly, possibly through another oracle process.
const WEATHER_CACHE_REFRESH: Duration = Duration::from_secs(5 * 60);
/// Current weather kept in memory: a station selection and period in a
/// reader's calendar each.
const MAX_CACHED_WEATHER: usize = 64;
/// Recently used current weather the warmer rebuilds after new data, at
/// most: the default airports in the readers' time zones, mostly.
const WARM_RECENT_WEATHER: usize = 16;
/// How often recent forecast files are checked for query-ready copies and
/// folds, besides after each upload: catches files another oracle process
/// received.
const PREPARE_FILES_INTERVAL: Duration = Duration::from_secs(10 * 60);

type TaskResult = Result<Result<()>, JoinError>;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

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
    forecast_cache: Mutex<Cache<String, String>>,
    weather_cache: Mutex<Cache<WeatherKey, Arc<Vec<WeatherDisplay>>>>,
    /// Counts arrivals of new data; cached values built from an older
    /// generation are stale.
    generation: AtomicU64,
    /// Wakes the cache warmer after new data is ready for queries.
    data_prepared: tokio::sync::Notify,
    stations: Arc<StationList>,
    background: Background,
    etl_slot: Arc<Semaphore>,
    /// This process, as a lease holder. Unique per start.
    instance: String,
    /// Wakes the task that prepares new data files for queries.
    files_added: tokio::sync::Notify,
    /// How far the first preparation of forecast files got.
    preparation: watch::Sender<Preparation>,
    metrics: Metrics,
}

/// The first pass that prepares recent forecast files for queries. Until
/// it ends, queries read the published files: seconds per request and
/// gigabytes of memory, so the cache warmer waits for it and readiness
/// answers 503.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preparation {
    Running,
    /// A pass failed and none has succeeded yet; the next one retries.
    Failed,
    /// A pass finished: copies and folds of every recent file exist.
    Done,
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
            forecast_cache: Mutex::new(Cache::new(MAX_CACHED_FORECASTS, FORECAST_CACHE_REFRESH)),
            weather_cache: Mutex::new(Cache::new(MAX_CACHED_WEATHER, WEATHER_CACHE_REFRESH)),
            generation: AtomicU64::new(0),
            data_prepared: tokio::sync::Notify::new(),
            stations: Arc::default(),
            background,
            etl_slot: Arc::new(Semaphore::new(1)),
            instance: format!("oracle-{}", uuid::Uuid::now_v7()),
            files_added: tokio::sync::Notify::new(),
            preparation: watch::Sender::new(Preparation::Running),
            metrics: Metrics::new(),
        }
    }

    /// Counters and gauges for the metrics listener. Kept up to date whether
    /// or not the listener runs.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Whether a preparation pass has made copies and folds of every recent
    /// forecast file since this process started.
    pub fn files_prepared(&self) -> bool {
        *self.preparation.borrow() == Preparation::Done
    }

    /// Records the end of a preparation pass. Once done, it stays done.
    fn preparation_ended(&self, succeeded: bool) {
        self.preparation.send_if_modified(|stage| {
            let next = match (*stage, succeeded) {
                (Preparation::Done, _) | (_, true) => Preparation::Done,
                (_, false) => Preparation::Failed,
            };
            std::mem::replace(stage, next) != next
        });
    }

    /// Waits until the first preparation pass ended, successfully or not.
    async fn first_preparation_ended(&self) {
        let mut stage = self.preparation.subscribe();
        // The sender lives as long as `self`, so this cannot fail.
        let _ = stage.wait_for(|stage| *stage != Preparation::Running).await;
    }

    /// The data generation cached values are fresh for. Read it before
    /// building a value to cache.
    pub fn data_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn cached_forecast(&self, key: &str) -> Cached<String> {
        let generation = self.data_generation();
        lock(&self.forecast_cache).get(&key.to_owned(), generation)
    }

    /// Caches a rendered forecast built from data `generation`. Callers
    /// pass only known station ids.
    pub(crate) fn cache_forecast(&self, key: String, html: String, generation: u64) {
        lock(&self.forecast_cache).insert(key, html, generation);
    }

    pub(crate) fn forecast_refresh_failed(&self, key: &str) {
        lock(&self.forecast_cache).refresh_failed(&key.to_owned());
    }

    pub(crate) fn cached_weather(&self, key: &WeatherKey) -> Cached<Arc<Vec<WeatherDisplay>>> {
        let generation = self.data_generation();
        lock(&self.weather_cache).get(key, generation)
    }

    pub(crate) fn cache_weather(
        &self,
        key: WeatherKey,
        weather: Arc<Vec<WeatherDisplay>>,
        generation: u64,
    ) {
        lock(&self.weather_cache).insert(key, weather, generation);
    }

    pub(crate) fn weather_refresh_failed(&self, key: &WeatherKey) {
        lock(&self.weather_cache).refresh_failed(key);
    }

    /// Current weather readers asked for lately, newest first.
    pub(crate) fn recent_weather(&self) -> Vec<WeatherKey> {
        let mut keys = lock(&self.weather_cache).recent_keys(MAX_CACHED_WEATHER as u64 * 4);
        keys.truncate(WARM_RECENT_WEATHER);
        keys
    }

    /// Runs `task` in the background, unless shutdown has begun.
    pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        if !self.background.is_stopping() {
            self.background.tasks.spawn(task);
        }
    }

    /// A data file was published: prepare it for queries.
    pub fn file_added(&self) {
        self.files_added.notify_one();
    }

    /// New data arrived: cached forecasts and weather become stale, served
    /// while they are rebuilt, and the station list is read again.
    pub fn new_data(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
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

    /// Rereads the station list in the background, one refresh at a time.
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
                Ok(true) => state.metrics.set_etl_lease_held(true),
                Ok(false) => {
                    state.metrics.set_etl_lease_held(false);
                    info!("another oracle process runs processing; skipped {etl_process_id}");
                    return;
                }
                Err(e) => {
                    state.metrics.etl_failed();
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
                        Ok(true) => state.metrics.set_etl_lease_held(true),
                        Ok(false) => {
                            state.metrics.set_etl_lease_held(false);
                            warn!("another oracle process took the processing lease");
                        }
                        Err(e) => warn!("cannot renew the processing lease: {e}"),
                    }
                }
            };
            let result = tokio::select! {
                result = state.oracle.etl_data(etl_process_id) => result,
                () = renewal => unreachable!("lease renewal never ends"),
            };
            match result {
                Ok(summary) => {
                    state.metrics.etl_completed(
                        summary.attested,
                        summary.failed,
                        time::OffsetDateTime::now_utc(),
                    );
                    if summary.failed == 0 {
                        info!("completed etl process: {etl_process_id}");
                    } else {
                        warn!(
                            "etl process {etl_process_id}: {} events failed",
                            summary.failed
                        );
                    }
                }
                Err(e) => {
                    state.metrics.etl_failed();
                    error!("failed etl process: {etl_process_id} {e:#}");
                }
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
        .route("/fragments/weather", get(weather_handler))
        .route("/fragments/forecast/{station_id}", get(forecast_handler))
        .route("/fragments/station/{station_id}", get(station_handler))
        .layer(middleware::from_fn(content_security_policy));

    Router::new()
        .merge(ui)
        .route("/assets/{file}", get(serve_asset))
        // Probes
        .route("/health", get(health))
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
        // Pages, fragments and JSON shrink about twentyfold; the weather
        // list is 160 KB of repetitive HTML. Assets arrive gzipped already
        // and are passed through, and Parquet files are compressed inside.
        .layer(CompressionLayer::new().compress_when(
            DefaultPredicate::new().and(NotForContentType::const_new("application/parquet")),
        ))
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
    /// The metrics listener, when `metrics_bind` is set. Stops with HTTP.
    metrics: Option<JoinHandle<Result<()>>>,
    metrics_address: Option<SocketAddr>,
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
            metrics: None,
            metrics_address: None,
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
            let metrics_listener = match configuration.metrics_listen {
                Some(metrics_listen) => {
                    let listener = TcpListener::bind(metrics_listen)
                        .await
                        .with_context(|| format!("bind metrics listener {metrics_listen}"))?;
                    let address = listener
                        .local_addr()
                        .context("read metrics listener address")?;
                    Some((listener, address))
                }
                None => None,
            };
            Ok::<_, anyhow::Error>((state, listener, address, metrics_listener))
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
        let (state, listener, address, metrics_listener) = match prepared {
            Ok(Some(prepared)) if !runtime.requested.is_cancelled() => prepared,
            result => {
                // Startup failures and signals must still close SQLite cleanly.
                let cleanup = runtime.shutdown().await;
                return first_failure(result.map(|_| ()), cleanup).map(|()| None);
            }
        };
        spawn_file_preparation(&state);
        spawn_cache_warmer(&state);
        spawn_etl_schedule(&state, configuration.etl_interval);
        spawn_lease_release(&state);
        if let Some((listener, address)) = metrics_listener {
            runtime.metrics = Some(spawn_http(
                listener,
                metrics::router(state.clone()),
                runtime.http_shutdown.clone(),
            ));
            runtime.metrics_address = Some(address);
        }
        runtime.http = Some(spawn_http(
            listener,
            app(state),
            runtime.http_shutdown.clone(),
        ));
        info!("NOAA Oracle listening on http://{address}");
        info!("  Docs:         http://{address}/docs");
        info!("  Weather data: {}", configuration.weather_dir.display());
        info!("  Event DB:     {}", configuration.event_dir.display());
        if let Some(address) = runtime.metrics_address {
            info!("  Metrics:      http://{address}/metrics");
        }
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
            completion = wait_for_task(&mut self.metrics) => {
                task_failure("metrics", completion, true)
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
                if self.metrics.is_some() {
                    let _ = wait_for_task(&mut self.metrics).await;
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
        let metrics_failure = if self.metrics.is_some() {
            task_failure("metrics", wait_for_task(&mut self.metrics).await, false)
        } else {
            None
        };
        let http_failure = first_failure(
            http_failure.map_or(Ok(()), Err),
            metrics_failure.map_or(Ok(()), Err),
        )
        .err();
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
        for task in [&self.http, &self.metrics, &self.writer]
            .into_iter()
            .flatten()
        {
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

/// Makes query-ready copies and folds of new forecast files (see
/// [`weather_data::DerivedForecasts`] and [`weather_data::Folds`]): at
/// start, after each upload, and every [`PREPARE_FILES_INTERVAL`]. Queries
/// read the published files until then, with the same results. Stops
/// between files on shutdown. The end of each pass is recorded for
/// readiness and the cache warmer (see [`Preparation`]).
fn spawn_file_preparation(state: &Arc<AppState>) {
    let state = state.clone();
    let stopping = state.background.stopping.clone();
    state.background.tasks.clone().spawn(async move {
        loop {
            let started = std::time::Instant::now();
            let result = state.weather_db.prepare_files(&stopping).await;
            match &result {
                Ok(0) => {}
                Ok(made) => {
                    info!(
                        "prepared {made} forecast files in {:.1}s",
                        started.elapsed().as_secs_f64()
                    );
                    // Possibly files another oracle process received.
                    state.new_data();
                }
                Err(error) => warn!("cannot prepare forecast files for queries: {error}"),
            }
            // A pass cut short by shutdown did not prepare every file.
            if !stopping.is_cancelled() {
                state.preparation_ended(result.is_ok());
                state.data_prepared.notify_one();
            }
            tokio::select! {
                biased;
                () = stopping.cancelled() => break,
                () = state.files_added.notified() => {}
                () = tokio::time::sleep(PREPARE_FILES_INTERVAL) => {}
            }
        }
    });
}

/// Warms the caches once the first preparation pass has ended, then after
/// each pass that follows new data, and every [`FORECAST_CACHE_REFRESH`].
/// Warming from the published files before that would fill every query
/// slot for minutes and take gigabytes of memory; from copies and folds it
/// takes seconds. Readers get the values warmed before while it runs.
fn spawn_cache_warmer(state: &Arc<AppState>) {
    let state = state.clone();
    let stopping = state.background.stopping.clone();
    state.background.tasks.clone().spawn(async move {
        tokio::select! {
            biased;
            () = stopping.cancelled() => return,
            () = state.first_preparation_ended() => {}
        }
        let mut warmed = state.data_generation();
        warm_caches(&state).await;
        let mut interval = tokio::time::interval(FORECAST_CACHE_REFRESH);
        interval.tick().await; // the first tick completes immediately
        loop {
            tokio::select! {
                biased;
                () = stopping.cancelled() => break,
                _ = interval.tick() => {}
                () = state.data_prepared.notified() => {
                    if state.data_generation() == warmed {
                        continue;
                    }
                }
            }
            warmed = state.data_generation();
            warm_caches(&state).await;
        }
    });
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
        state.metrics.set_etl_lease_held(false);
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
