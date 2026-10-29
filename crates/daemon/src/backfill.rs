//! Bounded observation recovery using the normal collector and publisher.
//! Publication timestamps remain current; only the requested history moves back.

use crate::{
    Configuration, HistoryConfig, ObservationService, XmlFetcher, get_coordinates_with_evidence,
    publish::Publisher,
    source::{NoaaForecasts, NoaaObservations, Run, Source, run_source},
};
use anyhow::{Context, Result, ensure};
use slog::{Logger, info};
use std::{
    sync::Arc,
    time::{Duration as StdDuration, Instant},
};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, PartialEq)]
struct Window {
    // The existing collector subtracts one second and requests a closed interval.
    end: OffsetDateTime,
    hours: u32,
}

fn windows(end: OffsetDateTime, hours: u32) -> Result<Vec<Window>> {
    ensure!(
        (1..=96).contains(&hours),
        "backfill hours must be between 1 and 96"
    );
    let mut remaining = hours;
    let mut end = end.replace_nanosecond(0)?;
    let mut result = Vec::new();
    while remaining > 0 {
        let hours = remaining.min(6);
        result.push(Window { end, hours });
        end -= Duration::hours(i64::from(hours));
        remaining -= hours;
    }
    Ok(result)
}

/// Publish up to sixteen history windows, newest first. Each uses the normal
/// per-request limits, source archives, quality checks, signatures, and S3 archive.
/// Failed receipts stay visible; publication does not assert settlement coverage.
pub async fn run(
    configuration: &Configuration,
    hours: u32,
    fetcher: Arc<XmlFetcher>,
    publisher: &Publisher,
    logger: &Logger,
) -> Result<()> {
    let plan = windows(OffsetDateTime::now_utc(), hours)?;
    let (stations, catalog) = get_coordinates_with_evidence(fetcher.clone()).await?;
    let service = ObservationService::new(logger.clone(), fetcher.clone());
    let live_sources: Vec<Box<dyn Source>> = vec![
        Box::new(NoaaForecasts::new(
            fetcher.clone(),
            configuration.min_forecast_coverage,
            logger.clone(),
        )),
        Box::new(NoaaObservations::new(
            fetcher,
            configuration.observation_history.clone(),
            logger.clone(),
        )),
    ];
    let mut live_refreshed: Option<Instant> = None;
    let mut incomplete = 0;
    for window in plan {
        // Recovery can take hours. Refresh current data sequentially through
        // the same sources and limiter, while the scheduled collector is paused.
        if live_refreshed.is_none_or(|at| at.elapsed() >= StdDuration::from_secs(45 * 60)) {
            info!(
                logger,
                "backfill: refreshing current forecasts and observations"
            );
            for source in &live_sources {
                run_source(source.as_ref(), publisher, &configuration.data_dir, logger).await;
            }
            live_refreshed = Some(Instant::now());
        }
        // The filename records the actual collection time, never a backdated issue.
        let started_at = OffsetDateTime::now_utc();
        let run = Run {
            directory: configuration.data_dir.join(started_at.date().to_string()),
            started_at,
            stamp: started_at.format(&Rfc3339)?,
        };
        tokio::fs::create_dir_all(&run.directory).await?;
        let artifact = run.artifact("observations");
        ensure!(!artifact.path.exists(), "backfill output already exists");
        // Day-sized collections exhaust the shared evidence budget before
        // reaching the whole catalog. Bound each pass, not just each request.
        // The collector still splits capped responses and enforces its limits.
        let history = HistoryConfig {
            hours: window.hours,
            batch_size: 25,
        };
        let mut retry = Vec::new();
        info!(
            logger,
            "backfill: collecting {} hours ending before {}",
            window.hours,
            window.end.format(&Rfc3339)?
        );
        let report = tokio::time::timeout(
            StdDuration::from_secs(35 * 60),
            service.get_observations_to_file(
                &stations,
                &artifact.path.to_string_lossy(),
                window.end,
                &history,
                &catalog,
                &mut retry,
            ),
        )
        .await
        .context("backfill window exceeded its deadline")??;
        // Publish even partial evidence, exactly as scheduled collection does.
        // The Oracle decides whether each requested event has complete coverage.
        publisher.publish(&artifact).await?;
        incomplete += retry.len();
        info!(
            logger,
            "backfill: published {} rows; {} intervals still need recovery",
            report.written,
            retry.len()
        );
    }
    ensure!(
        incomplete == 0,
        "backfill published evidence but {incomplete} intervals still need recovery"
    );
    info!(
        logger,
        "backfill publication finished; verify Oracle coverage and eligibility"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn recovery_covers_the_requested_history_without_gaps_or_future_windows() {
        let now = datetime!(2026-10-29 00:35:12.123 UTC);
        for hours in 1..=96 {
            let plan = windows(now, hours).unwrap();
            assert!(plan.len() <= 16);
            let newest = plan[0].end - Duration::seconds(1);
            assert!(newest < now);
            let mut previous_start = newest;
            for window in plan {
                assert!((1..=6).contains(&window.hours));
                let end = window.end - Duration::seconds(1);
                assert_eq!(end, previous_start);
                previous_start = end - Duration::hours(i64::from(window.hours));
            }
            assert_eq!(newest - previous_start, Duration::hours(i64::from(hours)));
        }
        assert!(windows(now, 0).is_err());
        assert!(windows(now, 97).is_err());
    }
}
