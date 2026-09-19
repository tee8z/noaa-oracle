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
//! - Return an error instead of files when a run is too incomplete to
//!   publish; partial data would be attested as if it were complete.

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

/// NOAA weather: NDFD forecasts and METAR observations for the stations in
/// the aviationweather.gov station catalog.
pub struct NoaaWeather {
    fetcher: Arc<XmlFetcher>,
    min_forecast_coverage: f64,
    logger: Logger,
}

impl NoaaWeather {
    pub fn new(fetcher: Arc<XmlFetcher>, min_forecast_coverage: f64, logger: Logger) -> Self {
        Self {
            fetcher,
            min_forecast_coverage,
            logger,
        }
    }
}

#[async_trait]
impl Source for NoaaWeather {
    fn name(&self) -> &'static str {
        "noaa_weather"
    }

    async fn collect(&self, run: &Run) -> anyhow::Result<Vec<Artifact>> {
        let stations = get_coordinates(self.fetcher.clone()).await?;
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
        let observations = run.artifact("observations");
        let observed = ObservationService::new(self.logger.clone(), self.fetcher.clone())
            .get_observations_to_file(&stations, &observations.path.to_string_lossy())
            .await?;
        info!(
            self.logger,
            "noaa run: {} forecast rows for {} stations, {} observations ({} reports skipped)",
            report.rows,
            report.written_stations,
            observed.written,
            observed.skipped
        );
        Ok(vec![forecasts, observations])
    }
}
