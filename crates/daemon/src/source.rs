//! Data sources the daemon collects.
//!
//! A [`Source`] writes one run's datasets as parquet files and returns them
//! for publishing; scheduling, uploading, archiving, retries, and retention
//! are shared. The oracle reads the files and attests outcomes over them
//! through its matching `OutcomeSource`; see `docs/attestation.md`.
//!
//! Contract for implementations:
//! - Write each dataset to `{run.directory}/{dataset}_{run.stamp}.parquet`
//!   and name it the same way; the oracle accepts only names it knows.
//! - Parquet schemas are append-only history: never rename, remove,
//!   reorder, or retype a column; add new columns at the end as nullable.
//! - Missing source values stay null; never substitute zero or a previous
//!   value unless the source's documentation defines it.
//! - Forecasts require sufficient validated coverage before publication.
//!   Observations publish explicit failed/empty/complete coverage receipts;
//!   the oracle must require continuous successful coverage before signing.

use async_trait::async_trait;
use slog::{Logger, error, info};
use std::{path::PathBuf, sync::Arc, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    ForecastService, ObservationService, XmlFetcher, get_coordinates,
    publish::{Artifact, Publisher},
};

/// One run: where to write and the timestamp every file of the run shares.
pub struct Run {
    pub directory: PathBuf,
    pub started_at: OffsetDateTime,
    /// `started_at` as RFC 3339, used in file names.
    pub stamp: String,
}

impl Run {
    pub fn artifact(&self, dataset: &str) -> Artifact {
        let name = format!("{dataset}_{}.parquet", self.stamp);
        Artifact {
            path: self.directory.join(&name),
            name,
            generated_at: self.started_at,
        }
    }
}

#[async_trait]
pub trait Source: Send + Sync {
    /// Stable name for logs.
    fn name(&self) -> &'static str;

    /// Writes this run's datasets and returns them for publishing.
    async fn collect(&self, run: &Run) -> anyhow::Result<Vec<Artifact>>;
}

/// NDFD forecasts publish independently of observation collection.
pub struct NoaaForecasts {
    fetcher: Arc<XmlFetcher>,
    min_forecast_coverage: f64,
    logger: Logger,
}

impl NoaaForecasts {
    pub fn new(fetcher: Arc<XmlFetcher>, min_forecast_coverage: f64, logger: Logger) -> Self {
        Self {
            fetcher,
            min_forecast_coverage,
            logger,
        }
    }
}

#[async_trait]
impl Source for NoaaForecasts {
    fn name(&self) -> &'static str {
        "noaa_forecasts"
    }

    async fn collect(&self, run: &Run) -> anyhow::Result<Vec<Artifact>> {
        let catalog = get_coordinates(self.fetcher.clone()).await?;
        let listed = catalog.city_data.len();
        let stations = catalog.metar_stations();
        info!(
            self.logger,
            "noaa forecasts: {} of {} catalog stations report METARs",
            stations.city_data.len(),
            listed
        );
        let forecasts = run.artifact("forecasts");
        let report = ForecastService::new(self.logger.clone(), self.fetcher.clone())
            .get_forecasts_to_file(&stations, &forecasts.path.to_string_lossy())
            .await?;
        if report.coverage() < self.min_forecast_coverage {
            anyhow::bail!(
                "forecasts cover {}/{} stations ({:.0}%), below the {:.0}% minimum; not publishing this run",
                report.written_stations,
                report.expected_stations,
                report.coverage() * 100.0,
                self.min_forecast_coverage * 100.0
            );
        }
        info!(
            self.logger,
            "noaa forecasts: {} rows for {} stations", report.rows, report.written_stations
        );
        Ok(vec![forecasts])
    }
}

/// Historical observations publish before the independent forecast run. Failed
/// leaf requests remain visible as receipts without certifying their intervals.
pub struct NoaaObservations {
    fetcher: Arc<XmlFetcher>,
    history: crate::HistoryConfig,
    logger: Logger,
    /// Intervals earlier runs failed to certify. Each run's window reaches
    /// back only `history.hours`, so without these a failure in its oldest
    /// hour would never be requested again and could block settlement.
    retry: std::sync::Mutex<Vec<crate::HistoryQuery>>,
}

impl NoaaObservations {
    pub fn new(fetcher: Arc<XmlFetcher>, history: crate::HistoryConfig, logger: Logger) -> Self {
        Self {
            fetcher,
            history,
            logger,
            retry: Default::default(),
        }
    }
}

#[async_trait]
impl Source for NoaaObservations {
    fn name(&self) -> &'static str {
        "noaa_observations"
    }

    async fn collect(&self, run: &Run) -> anyhow::Result<Vec<Artifact>> {
        let observations = run.artifact("observations");
        let service = ObservationService::new(self.logger.clone(), self.fetcher.clone());
        // A copy: a run cut short by its timeout keeps the earlier list.
        let mut retry = self.retry.lock().unwrap().clone();
        let (stations, catalog) =
            match crate::coordinates::get_coordinates_with_evidence(self.fetcher.clone()).await {
                Ok(stations) => stations,
                Err(error) => {
                    let written = service.write_unavailable_history(
                        &observations.path.to_string_lossy(),
                        run.started_at,
                        &self.history,
                        format!("station catalog failed: {error:#}"),
                        &mut retry,
                    );
                    *self.retry.lock().unwrap() = retry;
                    written?;
                    return Ok(vec![observations]);
                }
            };
        let observed = service
            .get_observations_to_file(
                &stations,
                &observations.path.to_string_lossy(),
                run.started_at,
                &self.history,
                &catalog,
                &mut retry,
            )
            .await;
        if !retry.is_empty() {
            info!(
                self.logger,
                "{} failed observation intervals to request next run",
                retry.len()
            );
        }
        *self.retry.lock().unwrap() = retry;
        let observed = observed?;
        info!(
            self.logger,
            "noaa observation history: {} rows ({} rejected, {} unverified, {} unrepresentable)",
            observed.written,
            observed.rejected,
            observed.unverified,
            observed.skipped
        );
        Ok(vec![observations])
    }
}

/// A run that takes longer than this is abandoned and retried next tick.
const RUN_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// Collects one run from `source` and publishes it. A run that fails or
/// times out leaves no files behind, so partial data is never published by
/// a later retry.
pub async fn run_source(
    source: &dyn Source,
    publisher: &Publisher,
    data_dir: &std::path::Path,
    logger: &Logger,
) {
    // One timestamp for the directory and every file name of the run.
    let started_at = OffsetDateTime::now_utc();
    let Ok(stamp) = started_at.format(&Rfc3339) else {
        return;
    };
    let run = Run {
        directory: data_dir.join(started_at.date().to_string()),
        started_at,
        stamp,
    };
    if let Err(error) = tokio::fs::create_dir_all(&run.directory).await {
        error!(
            logger,
            "cannot create {}: {}",
            run.directory.display(),
            error
        );
        return;
    }
    let collected = tokio::time::timeout(RUN_TIMEOUT, source.collect(&run)).await;
    let artifacts = match collected {
        Ok(Ok(artifacts)) => artifacts,
        Ok(Err(error)) => {
            error!(logger, "{} run failed: {:#}", source.name(), error);
            discard_run(&run).await;
            return;
        }
        Err(_) => {
            error!(
                logger,
                "{} run timed out after {:?}",
                source.name(),
                RUN_TIMEOUT
            );
            discard_run(&run).await;
            return;
        }
    };
    for artifact in &artifacts {
        if let Err(error) = publisher.publish(artifact).await {
            error!(
                logger,
                "publishing {} failed: {:#}",
                artifact.name,
                anyhow::Error::from(error)
            );
        }
    }
}

/// Removes every file the run wrote.
async fn discard_run(run: &Run) {
    let Ok(mut entries) = tokio::fs::read_dir(&run.directory).await else {
        return;
    };
    let suffix = format!("_{}.parquet", run.stamp);
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().ends_with(&suffix) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RejectedForecast;

    #[async_trait]
    impl Source for RejectedForecast {
        fn name(&self) -> &'static str {
            "rejected_forecast"
        }

        async fn collect(&self, run: &Run) -> anyhow::Result<Vec<Artifact>> {
            // Forecast collection can write a file before its coverage check fails.
            tokio::fs::write(run.artifact("forecasts").path, b"PAR1rejectedPAR1").await?;
            anyhow::bail!("forecast coverage below its minimum")
        }
    }

    #[tokio::test]
    async fn rejected_collection_cannot_be_published_by_a_later_retry() {
        let directory = tempfile::tempdir().unwrap();
        let day = directory
            .path()
            .join(OffsetDateTime::now_utc().date().to_string());
        std::fs::create_dir(&day).unwrap();
        let existing = day.join("observations_2020-01-01T00:00:00Z.parquet");
        std::fs::write(&existing, b"existing evidence").unwrap();
        let logger = Logger::root(slog::Discard, slog::o!());
        let publisher = Publisher::new(
            reqwest::Url::parse("http://127.0.0.1:1").unwrap(),
            nostr::key::Keys::generate(),
            None,
            logger.clone(),
        )
        .unwrap();

        run_source(&RejectedForecast, &publisher, directory.path(), &logger).await;

        assert_eq!(std::fs::read_dir(&day).unwrap().count(), 1);
        assert_eq!(std::fs::read(existing).unwrap(), b"existing evidence");
    }
}
