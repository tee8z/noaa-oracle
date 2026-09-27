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
use slog::{Logger, info};
use std::{path::PathBuf, sync::Arc};
use time::OffsetDateTime;

use crate::{ForecastService, ObservationService, XmlFetcher, get_coordinates, publish::Artifact};

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
}

impl NoaaObservations {
    pub fn new(fetcher: Arc<XmlFetcher>, history: crate::HistoryConfig, logger: Logger) -> Self {
        Self {
            fetcher,
            history,
            logger,
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
        let (stations, catalog) =
            match crate::coordinates::get_coordinates_with_evidence(self.fetcher.clone()).await {
                Ok(stations) => stations,
                Err(error) => {
                    service.write_unavailable_history(
                        &observations.path.to_string_lossy(),
                        run.started_at,
                        &self.history,
                        format!("station catalog failed: {error:#}"),
                    )?;
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
            )
            .await?;
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
