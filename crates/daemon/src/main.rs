use anyhow::Context;
use daemon::{
    Cli, RateLimiter, S3Storage, XmlFetcher, keys,
    publish::Publisher,
    setup_logger,
    source::{NoaaWeather, Run, Source},
};
use slog::{Logger, error, info, warn};
use std::{sync::Arc, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// A run that takes longer than this is abandoned and retried next tick.
const RUN_TIMEOUT: Duration = Duration::from_secs(45 * 60);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let configuration = Cli::load()?.configuration()?;
    let logger = setup_logger(configuration.level);

    std::fs::create_dir_all(&configuration.data_dir)
        .with_context(|| format!("cannot create {}", configuration.data_dir.display()))?;
    if let Some(parent) = configuration.private_key.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let keys = keys::load_or_create(&configuration.private_key)?;
    info!(logger, "NOAA Daemon starting";
        "npub" => keys::npub(&keys),
        "oracle" => configuration.base_url.as_str(),
        "data_dir" => configuration.data_dir.display().to_string(),
        "interval_s" => configuration.interval.as_secs());
    info!(
        logger,
        "add this npub to the oracle's uploader_pubkeys: {}",
        keys::npub(&keys)
    );

    let s3 = match &configuration.s3 {
        Some(settings) => {
            info!(logger, "archiving to S3 bucket {}", settings.bucket);
            Some(
                S3Storage::new(
                    settings.bucket.clone(),
                    settings.endpoint.clone(),
                    logger.clone(),
                )
                .await?,
            )
        }
        None => None,
    };
    let publisher = Publisher::new(configuration.base_url.clone(), keys, s3, logger.clone())?;
    let rate_limiter = Arc::new(Mutex::new(RateLimiter::new(
        configuration.token_capacity,
        configuration.refill_period,
    )));
    let fetcher = Arc::new(XmlFetcher::new(
        logger.clone(),
        &configuration.user_agent,
        rate_limiter,
    )?);
    let sources: Vec<Box<dyn Source>> = vec![Box::new(NoaaWeather::new(
        fetcher,
        configuration.min_forecast_coverage,
        logger.clone(),
    ))];

    let stop = CancellationToken::new();
    tokio::spawn(stop_on_signal(stop.clone(), logger.clone()));
    let mut ticks = tokio::time::interval(configuration.interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            _ = ticks.tick() => {}
        }
        for source in &sources {
            tokio::select! {
                biased;
                () = stop.cancelled() => break,
                () = run_source(source.as_ref(), &publisher, &configuration.data_dir, &logger) => {}
            }
        }
        let unpublished = publisher.publish_pending(&configuration.data_dir).await;
        if unpublished > 0 {
            warn!(
                logger,
                "{} files are still unpublished; retrying next run", unpublished
            );
        }
        let pruned = publisher.prune(
            &configuration.data_dir,
            configuration.retention,
            OffsetDateTime::now_utc(),
        );
        if pruned > 0 {
            info!(logger, "pruned {} published files past retention", pruned);
        }
    }
    info!(logger, "NOAA Daemon stopped");
    Ok(())
}

/// Collects one run from `source` and publishes it. A run that fails or
/// times out leaves no files behind, so partial data is never published by
/// a later retry.
async fn run_source(
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

async fn stop_on_signal(stop: CancellationToken, logger: Logger) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut terminate), Ok(mut interrupt)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            error!(logger, "cannot listen for stop signals");
            return;
        };
        tokio::select! { _ = terminate.recv() => {}, _ = interrupt.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    info!(logger, "stop requested");
    stop.cancel();
}
