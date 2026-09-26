//! Prometheus metrics, served on a separate listener only when
//! `metrics_bind` is set; never on the public router.
//!
//! Counters are updated where the work happens (processing passes and
//! uploads). Gauges read from the event database and the weather directory
//! are computed when scraped, at most every [`REFRESH_INTERVAL`], so
//! frequent scrapes never add database load.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use log::warn;
use prometheus::{
    Encoder, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder,
};
use time::{Date, OffsetDateTime, macros::format_description};

use crate::{
    AppState,
    file_access::{FileKind, ParquetFileName},
};

/// How long values read from the database and the weather directory are
/// reused before a scrape reads them again.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// Values of the `state` label on `oracle_events`.
const EVENT_STATES: [&str; 5] = ["live", "running", "completed", "signed", "unlisted"];

/// Every metric the oracle exports, in its own registry.
pub struct Metrics {
    registry: Registry,
    etl_runs: IntCounterVec,
    events_attested: IntCounter,
    attestation_failures: IntCounter,
    last_etl_completed: IntGauge,
    etl_lease_held: IntGauge,
    uploads: IntCounterVec,
    events: IntGaugeVec,
    awaiting_attestation: IntGauge,
    oldest_awaiting_age: IntGauge,
    latest_forecast: IntGauge,
    latest_observation: IntGauge,
    /// When the scrape-time gauges were last read.
    refreshed_at: tokio::sync::Mutex<Option<Instant>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let metrics = Self {
            etl_runs: IntCounterVec::new(
                Opts::new(
                    "oracle_etl_runs_total",
                    "Processing passes by result: completed, or failed before processing events",
                ),
                &["result"],
            )
            .expect("valid metric"),
            events_attested: IntCounter::new(
                "oracle_events_attested_total",
                "Events this process attested",
            )
            .expect("valid metric"),
            attestation_failures: IntCounter::new(
                "oracle_event_attestation_failures_total",
                "Events whose scoring or attestation failed in a processing pass",
            )
            .expect("valid metric"),
            last_etl_completed: IntGauge::new(
                "oracle_last_etl_completed_timestamp_seconds",
                "Unix time the last processing pass completed; 0 if none has",
            )
            .expect("valid metric"),
            etl_lease_held: IntGauge::new(
                "oracle_etl_lease_held",
                "1 while this process holds the processing lease, 0 otherwise",
            )
            .expect("valid metric"),
            uploads: IntCounterVec::new(
                Opts::new("oracle_uploads_total", "Data files accepted by upload"),
                &["kind"],
            )
            .expect("valid metric"),
            events: IntGaugeVec::new(
                Opts::new(
                    "oracle_events",
                    "Listed events by state (live, running, completed, signed); \
                     unlisted counts unlisted events in any state",
                ),
                &["state"],
            )
            .expect("valid metric"),
            awaiting_attestation: IntGauge::new(
                "oracle_events_awaiting_attestation",
                "Events with entries whose observation window ended and that are not attested",
            )
            .expect("valid metric"),
            oldest_awaiting_age: IntGauge::new(
                "oracle_oldest_event_awaiting_attestation_age_seconds",
                "Seconds since the signing date of the oldest event awaiting attestation; \
                 0 while none is due",
            )
            .expect("valid metric"),
            latest_forecast: IntGauge::new(
                "oracle_latest_forecast_timestamp_seconds",
                "Generation time of the newest forecast file; 0 if there is none",
            )
            .expect("valid metric"),
            latest_observation: IntGauge::new(
                "oracle_latest_observation_timestamp_seconds",
                "Generation time of the newest observation file; 0 if there is none",
            )
            .expect("valid metric"),
            refreshed_at: tokio::sync::Mutex::new(None),
            registry,
        };
        let build_info = IntGaugeVec::new(
            Opts::new("oracle_build_info", "Oracle version; always 1"),
            &["version"],
        )
        .expect("valid metric");
        build_info
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);
        for result in ["completed", "failed"] {
            metrics.etl_runs.with_label_values(&[result]);
        }
        for kind in [FileKind::Forecasts, FileKind::Observations] {
            metrics.uploads.with_label_values(&[kind_label(kind)]);
        }
        for state in EVENT_STATES {
            metrics.events.with_label_values(&[state]);
        }
        let collectors: [Box<dyn prometheus::core::Collector>; 12] = [
            Box::new(build_info),
            Box::new(metrics.etl_runs.clone()),
            Box::new(metrics.events_attested.clone()),
            Box::new(metrics.attestation_failures.clone()),
            Box::new(metrics.last_etl_completed.clone()),
            Box::new(metrics.etl_lease_held.clone()),
            Box::new(metrics.uploads.clone()),
            Box::new(metrics.events.clone()),
            Box::new(metrics.awaiting_attestation.clone()),
            Box::new(metrics.oldest_awaiting_age.clone()),
            Box::new(metrics.latest_forecast.clone()),
            Box::new(metrics.latest_observation.clone()),
        ];
        for collector in collectors {
            metrics
                .registry
                .register(collector)
                .expect("metric names are unique");
        }
        metrics
    }

    /// A processing pass finished: `attested` events were signed and
    /// `failed` events could not be processed.
    pub fn etl_completed(&self, attested: usize, failed: usize, at: OffsetDateTime) {
        self.etl_runs.with_label_values(&["completed"]).inc();
        self.events_attested.inc_by(attested as u64);
        self.attestation_failures.inc_by(failed as u64);
        self.last_etl_completed.set(at.unix_timestamp());
    }

    /// A processing pass could not run its events at all.
    pub fn etl_failed(&self) {
        self.etl_runs.with_label_values(&["failed"]).inc();
    }

    pub fn set_etl_lease_held(&self, held: bool) {
        self.etl_lease_held.set(i64::from(held));
    }

    pub fn upload_accepted(&self, kind: FileKind) {
        self.uploads.with_label_values(&[kind_label(kind)]).inc();
    }

    /// Renders every metric in the Prometheus text format, first rereading
    /// the database and the weather directory if the last read is older
    /// than [`REFRESH_INTERVAL`]. A failed read keeps the previous values.
    pub async fn render(&self, state: &AppState) -> String {
        {
            let mut refreshed_at = self.refreshed_at.lock().await;
            if refreshed_at.is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL) {
                self.refresh(state).await;
                *refreshed_at = Some(Instant::now());
            }
        }
        self.encode()
    }

    /// Every metric in the Prometheus text format, as last read.
    pub fn encode(&self) -> String {
        let mut buffer = vec![];
        if let Err(error) = TextEncoder::new().encode(&self.registry.gather(), &mut buffer) {
            warn!("cannot encode metrics: {error}");
        }
        String::from_utf8(buffer).unwrap_or_default()
    }

    /// Rereads the gauges that come from the database and the weather
    /// directory.
    pub async fn refresh(&self, state: &AppState) {
        match state.oracle.event_counts(false).await {
            Ok(counts) => {
                let counts = [
                    counts.live,
                    counts.running,
                    counts.completed,
                    counts.signed,
                    counts.unlisted,
                ];
                for (label, count) in EVENT_STATES.into_iter().zip(counts) {
                    self.events
                        .with_label_values(&[label])
                        .set(i64::try_from(count).unwrap_or(i64::MAX));
                }
            }
            Err(error) => warn!("metrics: cannot count events: {error:#}"),
        }
        match state.oracle.awaiting_attestation().await {
            Ok(awaiting) => {
                self.awaiting_attestation
                    .set(i64::try_from(awaiting.count).unwrap_or(i64::MAX));
                let now = state.oracle.now().unix_timestamp();
                let age = awaiting
                    .oldest_signing_date
                    .map_or(0, |due| (now - due.unix_timestamp()).max(0));
                self.oldest_awaiting_age.set(age);
            }
            Err(error) => warn!("metrics: cannot count events awaiting attestation: {error:#}"),
        }
        let latest = latest_files(&state.weather_dir).await;
        self.latest_forecast
            .set(latest.forecast.map_or(0, OffsetDateTime::unix_timestamp));
        self.latest_observation
            .set(latest.observation.map_or(0, OffsetDateTime::unix_timestamp));
    }
}

fn kind_label(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Forecasts => "forecasts",
        FileKind::Observations => "observations",
    }
}

/// Generation times of the newest forecast and observation files.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LatestFiles {
    pub forecast: Option<OffsetDateTime>,
    pub observation: Option<OffsetDateTime>,
}

/// Finds the newest data files in `weather_dir`, reading date directories
/// newest first. Stops one directory after both kinds were found: a file
/// named with a UTC offset can sit in the directory of the day before.
pub async fn latest_files(weather_dir: &Path) -> LatestFiles {
    let mut latest = LatestFiles::default();
    let Ok(mut entries) = tokio::fs::read_dir(weather_dir).await else {
        return latest;
    };
    let date_format = format_description!("[year]-[month]-[day]");
    let mut days = vec![];
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some(day) = entry
            .file_name()
            .to_str()
            .and_then(|name| Date::parse(name, &date_format).ok())
        {
            days.push((day, entry.path()));
        }
    }
    days.sort_unstable_by_key(|(day, _)| std::cmp::Reverse(*day));
    let mut extra_directories = 1;
    for (_, directory) in days {
        let Ok(mut files) = tokio::fs::read_dir(directory).await else {
            continue;
        };
        while let Ok(Some(file)) = files.next_entry().await {
            let Some(parsed) = file
                .file_name()
                .to_str()
                .and_then(|name| ParquetFileName::parse(name).ok())
            else {
                continue;
            };
            let newest = match parsed.kind {
                FileKind::Forecasts => &mut latest.forecast,
                FileKind::Observations => &mut latest.observation,
            };
            if newest.is_none_or(|time| time < parsed.generated_at) {
                *newest = Some(parsed.generated_at);
            }
        }
        if latest.forecast.is_some() && latest.observation.is_some() {
            if extra_directories == 0 {
                break;
            }
            extra_directories -= 1;
        }
    }
    latest
}

/// The metrics listener's router: `GET /metrics` and nothing else.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .with_state(state)
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let body = state.metrics().render(&state).await;
    (
        StatusCode::OK,
        [(CONTENT_TYPE, TextEncoder::new().format_type().to_owned())],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn touch(directory: &Path, day: &str, name: &str) {
        let day = directory.join(day);
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join(name), b"PAR1PAR1").unwrap();
    }

    #[tokio::test]
    async fn the_newest_file_of_each_kind_is_found() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        assert_eq!(latest_files(root).await, LatestFiles::default());
        assert_eq!(
            latest_files(&root.join("missing")).await,
            LatestFiles::default()
        );

        touch(
            root,
            "2030-01-01",
            "observations_2030-01-01T10:00:00Z.parquet",
        );
        touch(root, "2030-01-01", "forecasts_2030-01-01T09:00:00Z.parquet");
        touch(root, "2030-01-02", "forecasts_2030-01-02T01:00:00Z.parquet");
        // Named with an offset: filed under its own date, a UTC day later.
        touch(
            root,
            "2030-01-01",
            "observations_2030-01-01T22:00:00-05:00.parquet",
        );
        touch(root, "2030-01-02", "notes.txt");
        std::fs::create_dir_all(root.join("derived")).unwrap();
        touch(root, "2029-12-31", "forecasts_2029-12-31T00:00:00Z.parquet");

        assert_eq!(
            latest_files(root).await,
            LatestFiles {
                forecast: Some(datetime!(2030-01-02 01:00 UTC)),
                observation: Some(datetime!(2030-01-02 03:00 UTC)),
            }
        );
    }

    #[test]
    fn every_family_is_registered_before_any_work() {
        let text = Metrics::new().encode();
        for family in [
            "oracle_build_info",
            "oracle_etl_runs_total",
            "oracle_events_attested_total",
            "oracle_event_attestation_failures_total",
            "oracle_last_etl_completed_timestamp_seconds",
            "oracle_etl_lease_held",
            "oracle_uploads_total",
            "oracle_events",
            "oracle_events_awaiting_attestation",
            "oracle_oldest_event_awaiting_attestation_age_seconds",
            "oracle_latest_forecast_timestamp_seconds",
            "oracle_latest_observation_timestamp_seconds",
        ] {
            assert!(text.contains(&format!("# TYPE {family} ")), "{family}");
        }
        assert!(text.contains(&format!(
            "oracle_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(text.contains("oracle_etl_runs_total{result=\"failed\"} 0"));
        assert!(text.contains("oracle_uploads_total{kind=\"forecasts\"} 0"));
    }
}
