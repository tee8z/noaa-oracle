//! Bounded observation recovery using the normal collector and publisher.
//! Publication timestamps remain current; only the requested history moves back.

use crate::{
    Configuration, HistoryConfig, ObservationService, XmlFetcher, get_coordinates_with_evidence,
    publish::Publisher, source::Run,
};
use anyhow::{Context, Result, ensure};
use slog::{Logger, info};
use std::{sync::Arc, time::Duration as StdDuration};
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
        let hours = remaining.min(24);
        result.push(Window { end, hours });
        end -= Duration::hours(i64::from(hours));
        remaining -= hours;
    }
    Ok(result)
}

/// Publish up to four history windows, newest first. Each uses the normal
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
    let service = ObservationService::new(logger.clone(), fetcher);
    let mut incomplete = 0;
    for window in plan {
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
        // Ten stations normally fit a full day below AWC's 400-report cap.
        // The existing collector still splits capped responses and enforces budgets.
        let history = HistoryConfig {
            hours: window.hours,
            batch_size: 10,
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
        for hours in [1, 23, 24, 25, 72, 96] {
            let plan = windows(now, hours).unwrap();
            assert!(plan.len() <= 4);
            let newest = plan[0].end - Duration::seconds(1);
            assert!(newest < now);
            let mut previous_start = newest;
            for window in plan {
                assert!((1..=24).contains(&window.hours));
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
