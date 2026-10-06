//! Prometheus metrics, served on a separate listener only when
//! `metrics_bind` is set; never on the public router.
//!
//! Counters are updated where the work happens (processing passes and
//! uploads). Gauges read from the event database and the weather directory
//! are computed when scraped, at most every [`REFRESH_INTERVAL`], so
//! frequent scrapes never add database load.
//!
//! `oracle_events_awaiting_attestation` and
//! `oracle_oldest_event_awaiting_attestation_age_seconds` count every event
//! the oracle has yet to attest. `oracle_events_blocked_on_source_coverage`
//! counts those among them whose latest check failed on the published data:
//! observations that do not cover the window, such as a report the upstream
//! source never published, or a scored metric without a forecast baseline.
//! `oracle_events_unsettleable` counts the latter alone: they can never be
//! attested, and their contracts end through expiry.
//! `oracle_oldest_attestable_event_age_seconds` gives the age of the oldest
//! event that does not wait on observations, so an alert on it points at
//! the oracle itself or at an event only an operator can resolve.
//!
//! `oracle_eligible_stations` counts the stations eligible over the default
//! history and window length (3 days, 24 hours), judged after each
//! collection run whatever requests ask for. `oracle_eligibility_reports`
//! counts the reports read ahead for those judgments.
//!
//! `process_resident_memory_bytes` and `oracle_cache_entries` are read
//! when scraped; `oracle_weather_requests_turned_away_total` counts weather
//! requests answered 503 because too many were waiting or one ran too long,
//! and `oracle_heavy_requests_turned_away_total` those whose heavy work
//! (eligible lists, discovery, window planning) found no turn or was not
//! done in time (see [`crate::heavy`]).
//!
//! `oracle_nostr_published_total` and `oracle_nostr_publish_failures_total`
//! count deliveries of announcements and attestations to Nostr relays, one
//! per event and relay; `oracle_nostr_outbox_depth` counts those still
//! waiting, due or backing off after a failure.

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
    database::AwaitingAttestation,
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
    blocked_on_source_coverage: IntGauge,
    unsettleable: IntGauge,
    oldest_attestable_age: IntGauge,
    expired_unsigned: IntGauge,
    latest_forecast: IntGauge,
    latest_observation: IntGauge,
    eligible_stations: IntGauge,
    eligibility_reports: IntGauge,
    turned_away: IntCounter,
    heavy_turned_away: IntCounter,
    resident_memory: IntGauge,
    cache_entries: IntGaugeVec,
    cache_bytes: IntGaugeVec,
    nostr_published: IntCounter,
    nostr_failures: IntCounter,
    nostr_outbox_depth: IntGauge,
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
            blocked_on_source_coverage: IntGauge::new(
                "oracle_events_blocked_on_source_coverage",
                "Events awaiting attestation whose latest check failed because the published \
                 observations or scored forecast baselines do not cover their window",
            )
            .expect("valid metric"),
            unsettleable: IntGauge::new(
                "oracle_events_unsettleable",
                "Events awaiting expiry because a scored forecast baseline is missing",
            )
            .expect("valid metric"),
            oldest_attestable_age: IntGauge::new(
                "oracle_oldest_attestable_event_age_seconds",
                "Seconds since the signing date of the oldest event awaiting attestation \
                 that is not blocked on source coverage; 0 while none is due",
            )
            .expect("valid metric"),
            expired_unsigned: IntGauge::new(
                "oracle_events_expired_unsigned",
                "Events with entries that reached their DLC expiry without an attestation; \
                 their contracts refund through the expiry path",
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
            eligible_stations: IntGauge::new(
                "oracle_eligible_stations",
                "Stations eligible for a 24-hour competition over the default 3 days of \
                 history, judged after each collection run; 0 until the first judgment",
            )
            .expect("valid metric"),
            eligibility_reports: IntGauge::new(
                "oracle_eligibility_reports",
                "Observation reports read ahead for eligibility judgments",
            )
            .expect("valid metric"),
            turned_away: IntCounter::new(
                "oracle_weather_requests_turned_away_total",
                "Weather requests answered 503 because too many were waiting for a turn, \
                 or because one ran longer than the request timeout",
            )
            .expect("valid metric"),
            heavy_turned_away: IntCounter::new(
                "oracle_heavy_requests_turned_away_total",
                "Requests answered 503 because the heavy work they needed found no turn \
                 or was not done within the request timeout",
            )
            .expect("valid metric"),
            resident_memory: IntGauge::new(
                "process_resident_memory_bytes",
                "Resident memory size of the oracle process in bytes",
            )
            .expect("valid metric"),
            cache_bytes: IntGaugeVec::new(
                Opts::new(
                    "oracle_cache_bytes",
                    "Estimated retained key and value allocation bytes by cache",
                ),
                &["cache"],
            )
            .expect("valid metric"),
            cache_entries: IntGaugeVec::new(
                Opts::new("oracle_cache_entries", "Entries in each in-memory cache"),
                &["cache"],
            )
            .expect("valid metric"),
            nostr_published: IntCounter::new(
                "oracle_nostr_published_total",
                "Announcements and attestations accepted by a Nostr relay, per relay",
            )
            .expect("valid metric"),
            nostr_failures: IntCounter::new(
                "oracle_nostr_publish_failures_total",
                "Attempts to publish to a Nostr relay that failed and will be retried",
            )
            .expect("valid metric"),
            nostr_outbox_depth: IntGauge::new(
                "oracle_nostr_outbox_depth",
                "Publications to Nostr relays not yet accepted",
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
        let collectors: Vec<Box<dyn prometheus::core::Collector>> = vec![
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
            Box::new(metrics.blocked_on_source_coverage.clone()),
            Box::new(metrics.unsettleable.clone()),
            Box::new(metrics.oldest_attestable_age.clone()),
            Box::new(metrics.expired_unsigned.clone()),
            Box::new(metrics.latest_forecast.clone()),
            Box::new(metrics.latest_observation.clone()),
            Box::new(metrics.eligible_stations.clone()),
            Box::new(metrics.eligibility_reports.clone()),
            Box::new(metrics.turned_away.clone()),
            Box::new(metrics.heavy_turned_away.clone()),
            Box::new(metrics.resident_memory.clone()),
            Box::new(metrics.cache_entries.clone()),
            Box::new(metrics.cache_bytes.clone()),
            Box::new(metrics.nostr_published.clone()),
            Box::new(metrics.nostr_failures.clone()),
            Box::new(metrics.nostr_outbox_depth.clone()),
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

    /// The default list of eligible stations was judged with `count`
    /// stations.
    pub fn set_eligible_stations(&self, count: usize) {
        self.eligible_stations
            .set(i64::try_from(count).unwrap_or(i64::MAX));
    }

    /// `count` reports were read ahead for eligibility judgments.
    pub fn set_eligibility_reports(&self, count: usize) {
        self.eligibility_reports
            .set(i64::try_from(count).unwrap_or(i64::MAX));
    }

    /// A weather request was turned away while too many waited, or ran
    /// too long.
    pub fn weather_request_turned_away(&self) {
        self.turned_away.inc();
    }

    /// A request's heavy work found no turn or was not done in time.
    pub fn heavy_request_turned_away(&self) {
        self.heavy_turned_away.inc();
    }

    /// A publishing pass delivered `published` events to relays and failed
    /// `failed` deliveries.
    pub fn nostr_pass(&self, published: u64, failed: u64) {
        self.nostr_published.inc_by(published);
        self.nostr_failures.inc_by(failed);
    }

    pub fn set_nostr_outbox_depth(&self, depth: u64) {
        self.nostr_outbox_depth
            .set(i64::try_from(depth).unwrap_or(i64::MAX));
    }

    pub fn upload_accepted(&self, kind: FileKind) {
        self.uploads.with_label_values(&[kind_label(kind)]).inc();
    }

    /// Renders every metric in the Prometheus text format, first rereading
    /// the database and the weather directory if the last read is older
    /// than [`REFRESH_INTERVAL`]. A failed read keeps the previous values.
    pub async fn render(&self, state: &AppState) -> String {
        self.refresh_if_stale(state).await;
        self.encode()
    }

    async fn refresh_if_stale(&self, state: &AppState) {
        let mut refreshed_at = self.refreshed_at.lock().await;
        if refreshed_at.is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL) {
            self.refresh(state).await;
            *refreshed_at = Some(Instant::now());
        }
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
            Ok(awaiting) => self.set_awaiting(&awaiting, state.oracle.now()),
            Err(error) => warn!("metrics: cannot count events awaiting attestation: {error:#}"),
        }
        if let Some(publication) = state.oracle.publication() {
            match state
                .database
                .publication_backlog(&publication.relays)
                .await
            {
                Ok(depth) => self.set_nostr_outbox_depth(depth),
                Err(error) => warn!("metrics: cannot count the nostr outbox: {error:#}"),
            }
        }
        let latest = latest_files(&state.weather_dir).await;
        self.latest_forecast
            .set(latest.forecast.map_or(0, OffsetDateTime::unix_timestamp));
        self.latest_observation
            .set(latest.observation.map_or(0, OffsetDateTime::unix_timestamp));
        if let Some(bytes) = resident_memory().await {
            self.resident_memory
                .set(i64::try_from(bytes).unwrap_or(i64::MAX));
        }
        for (cache, bytes) in state.cache_bytes() {
            self.cache_bytes
                .with_label_values(&[cache])
                .set(i64::try_from(bytes).unwrap_or(i64::MAX));
        }
        for (cache, entries) in state.cache_entries() {
            self.cache_entries
                .with_label_values(&[cache])
                .set(i64::try_from(entries).unwrap_or(i64::MAX));
        }
    }

    /// Sets the gauges of events awaiting attestation as of `now`.
    fn set_awaiting(&self, awaiting: &AwaitingAttestation, now: OffsetDateTime) {
        let count = |value: usize| i64::try_from(value).unwrap_or(i64::MAX);
        let now = now.unix_timestamp();
        let age =
            |due: Option<OffsetDateTime>| due.map_or(0, |due| (now - due.unix_timestamp()).max(0));
        self.awaiting_attestation.set(count(awaiting.count));
        self.expired_unsigned.set(count(awaiting.expired));
        self.unsettleable.set(count(awaiting.unsettleable));
        self.oldest_awaiting_age
            .set(age(awaiting.oldest_signing_date));
        self.blocked_on_source_coverage
            .set(count(awaiting.blocked_on_source_coverage));
        self.oldest_attestable_age
            .set(age(awaiting.oldest_attestable_signing_date));
    }
}

/// The process's resident memory in bytes, from `/proc`; `None` where
/// there is no `/proc`.
async fn resident_memory() -> Option<u64> {
    let status = tokio::fs::read_to_string("/proc/self/status").await.ok()?;
    resident_kilobytes(&status).map(|kilobytes| kilobytes * 1024)
}

/// The `VmRSS` line of a `/proc/<pid>/status` file, in kilobytes.
fn resident_kilobytes(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()
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

/// The private listener serves metrics and the operator view. Keep it behind
/// the operator access policy; the public listener never installs this marker.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .with_state(state.clone())
        .merge(crate::startup::app(state))
        .layer(axum::Extension(crate::routes::ui::OperatorView))
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

    /// One event waits on observations the source never published, the
    /// other on the oracle: only the second ages the attestable gauge.
    #[test]
    fn events_blocked_on_source_coverage_are_counted_apart() {
        let metrics = Metrics::new();
        let now = datetime!(2026-10-01 12:00 UTC);
        metrics.set_awaiting(
            &AwaitingAttestation {
                count: 2,
                expired: 0,
                unsettleable: 0,
                oldest_signing_date: Some(now - time::Duration::hours(5)),
                blocked_on_source_coverage: 1,
                oldest_attestable_signing_date: Some(now - time::Duration::minutes(10)),
            },
            now,
        );
        let text = metrics.encode();
        for (series, value) in [
            ("oracle_events_awaiting_attestation", 2),
            (
                "oracle_oldest_event_awaiting_attestation_age_seconds",
                18_000,
            ),
            ("oracle_events_blocked_on_source_coverage", 1),
            ("oracle_oldest_attestable_event_age_seconds", 600),
        ] {
            assert!(text.contains(&format!("\n{series} {value}\n")), "{series}");
        }
        metrics.set_awaiting(
            &AwaitingAttestation {
                count: 1,
                oldest_signing_date: Some(now - time::Duration::hours(5)),
                blocked_on_source_coverage: 1,
                ..AwaitingAttestation::default()
            },
            now,
        );
        let text = metrics.encode();
        assert!(text.contains("\noracle_oldest_attestable_event_age_seconds 0\n"));
        assert!(text.contains("\noracle_oldest_event_awaiting_attestation_age_seconds 18000\n"));
    }

    #[test]
    fn resident_memory_is_read_from_the_status_file() {
        let status = "Name:\toracle\nVmPeak:\t 9000 kB\nVmRSS:\t   2048 kB\nRssAnon:\t 1024 kB\n";
        assert_eq!(resident_kilobytes(status), Some(2048));
        assert_eq!(resident_kilobytes("Name:\toracle\n"), None);
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
            "oracle_events_blocked_on_source_coverage",
            "oracle_oldest_attestable_event_age_seconds",
            "oracle_latest_forecast_timestamp_seconds",
            "oracle_latest_observation_timestamp_seconds",
            "oracle_eligible_stations",
            "oracle_eligibility_reports",
            "oracle_weather_requests_turned_away_total",
            "oracle_heavy_requests_turned_away_total",
            "process_resident_memory_bytes",
            "oracle_nostr_published_total",
            "oracle_nostr_publish_failures_total",
            "oracle_nostr_outbox_depth",
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
