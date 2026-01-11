use daemon::{
    create_folder, get_config_info, get_coordinates, save_forecasts, save_observations,
    send_parquet_files, setup_logger, subfolder_exists, Cli, ForecastService, ObservationService,
    RateLimiter, XmlFetcher,
};
use slog::{debug, error, info, Logger};
use std::{sync::Arc, time::Duration};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::Mutex;
use tokio::time::interval;

#[cfg(feature = "s3")]
use daemon::{upload_to_s3, S3Storage};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let cli = get_config_info();
    let logger = setup_logger(&cli);

    info!(logger, "NOAA Daemon starting...");
    info!(logger, "  Oracle URL: {}", cli.base_url());
    info!(logger, "  Data dir: {}", cli.data_dir());
    info!(logger, "  Fetch interval: {} seconds", cli.sleep_interval());

    #[cfg(feature = "s3")]
    if cli.s3_bucket.is_some() {
        info!(logger, "  S3 bucket: {}", cli.s3_bucket.as_ref().unwrap());
        if let Some(ref endpoint) = cli.s3_endpoint {
            info!(logger, "  S3 endpoint: {}", endpoint);
        }
    }

    let rate_limiter = Arc::new(Mutex::new(RateLimiter::new(
        cli.token_capacity(),
        cli.refill_rate(),
    )));

    #[cfg(feature = "s3")]
    let s3_storage = if let Some(ref bucket) = cli.s3_bucket {
        Some(
            S3Storage::new(bucket.clone(), cli.s3_endpoint.clone(), logger.clone())
                .await
                .expect("Failed to initialize S3 storage"),
        )
    } else {
        None
    };

    #[cfg(feature = "s3")]
    process_weather_data_hourly(cli, logger, Arc::clone(&rate_limiter), s3_storage).await;

    #[cfg(not(feature = "s3"))]
    process_weather_data_hourly(cli, logger, Arc::clone(&rate_limiter)).await;

    Ok(())
}

#[cfg(feature = "s3")]
async fn process_weather_data_hourly(
    cli: Cli,
    logger: Logger,
    rate_limit: Arc<Mutex<RateLimiter>>,
    s3_storage: Option<S3Storage>,
) {
    let sleep_between_checks = cli.sleep_interval();
    info!(
        logger,
        "Wait time between data pulls: {} seconds", sleep_between_checks
    );

    let mut check_channel_interval = interval(Duration::from_secs(sleep_between_checks));
    loop {
        tokio::select! {
            _ = check_channel_interval.tick() => {
                match process_data(cli.clone(), logger.clone(), rate_limit.clone(), s3_storage.as_ref()).await {
                    Ok(_) => info!(logger, "Finished processing data, waiting {} seconds for next run", sleep_between_checks),
                    Err(err) => error!(&logger, "Error processing data: {}", err)
                }
            }
        }
    }
}

#[cfg(not(feature = "s3"))]
async fn process_weather_data_hourly(
    cli: Cli,
    logger: Logger,
    rate_limit: Arc<Mutex<RateLimiter>>,
) {
    let sleep_between_checks = cli.sleep_interval();
    info!(
        logger,
        "Wait time between data pulls: {} seconds", sleep_between_checks
    );

    let mut check_channel_interval = interval(Duration::from_secs(sleep_between_checks));
    loop {
        tokio::select! {
            _ = check_channel_interval.tick() => {
                match process_data(cli.clone(), logger.clone(), rate_limit.clone()).await {
                    Ok(_) => info!(logger, "Finished processing data, waiting {} seconds for next run", sleep_between_checks),
                    Err(err) => error!(&logger, "Error processing data: {}", err)
                }
            }
        }
    }
}

#[cfg(feature = "s3")]
async fn process_data(
    cli: Cli,
    logger: Logger,
    rate_limiter: Arc<Mutex<RateLimiter>>,
    s3_storage: Option<&S3Storage>,
) -> Result<(), anyhow::Error> {
    let logger_cpy = &logger.clone();
    let fetcher = Arc::new(XmlFetcher::new(
        logger.clone(),
        cli.user_agent(),
        rate_limiter,
    ));

    let city_weather_coordinates = get_coordinates(fetcher.clone()).await?;
    debug!(logger_cpy, "coordinates: {}", city_weather_coordinates);

    let forecast_service = ForecastService::new(logger.clone(), fetcher.clone());
    let forecasts = forecast_service
        .get_forecasts(&city_weather_coordinates)
        .await?;
    debug!(logger_cpy, "forecasts count: {}", forecasts.len());

    let observation_service = ObservationService::new(logger, fetcher);
    let observations = observation_service
        .get_observations(&city_weather_coordinates)
        .await?;
    debug!(logger_cpy, "observations count: {:?}", observations.len());

    let current_utc_time: String = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let root_path = cli.data_dir();
    create_folder(&root_path, logger_cpy);

    let current_date = OffsetDateTime::now_utc().date();
    let subfolder = format!("{}/{}", root_path, current_date);
    if !subfolder_exists(&subfolder) {
        create_folder(&subfolder, logger_cpy)
    }

    let forecast_parquet = save_forecasts(
        forecasts,
        &subfolder,
        format!("{}_{}", "forecasts", current_utc_time),
    );
    let observation_parquet = save_observations(
        observations,
        &subfolder,
        format!("{}_{}", "observations", current_utc_time),
    );

    if let Some(s3) = s3_storage {
        let date_folder = current_date.to_string();
        upload_to_s3(
            s3,
            logger_cpy,
            &observation_parquet,
            &forecast_parquet,
            &date_folder,
        )
        .await?;
    } else {
        send_parquet_files(&cli, logger_cpy, observation_parquet, forecast_parquet).await?;
    }

    Ok(())
}

#[cfg(not(feature = "s3"))]
async fn process_data(
    cli: Cli,
    logger: Logger,
    rate_limiter: Arc<Mutex<RateLimiter>>,
) -> Result<(), anyhow::Error> {
    let logger_cpy = &logger.clone();
    let fetcher = Arc::new(XmlFetcher::new(
        logger.clone(),
        cli.user_agent(),
        rate_limiter,
    ));

    let city_weather_coordinates = get_coordinates(fetcher.clone()).await?;
    debug!(logger_cpy, "coordinates: {}", city_weather_coordinates);

    let forecast_service = ForecastService::new(logger.clone(), fetcher.clone());
    let forecasts = forecast_service
        .get_forecasts(&city_weather_coordinates)
        .await?;
    debug!(logger_cpy, "forecasts count: {}", forecasts.len());

    let observation_service = ObservationService::new(logger, fetcher);
    let observations = observation_service
        .get_observations(&city_weather_coordinates)
        .await?;
    debug!(logger_cpy, "observations count: {:?}", observations.len());

    let current_utc_time: String = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let root_path = cli.data_dir();
    create_folder(&root_path, logger_cpy);

    let current_date = OffsetDateTime::now_utc().date();
    let subfolder = format!("{}/{}", root_path, current_date);
    if !subfolder_exists(&subfolder) {
        create_folder(&subfolder, logger_cpy)
    }

    let forecast_parquet = save_forecasts(
        forecasts,
        &subfolder,
        format!("{}_{}", "forecasts", current_utc_time),
    );
    let observation_parquet = save_observations(
        observations,
        &subfolder,
        format!("{}_{}", "observations", current_utc_time),
    );

    send_parquet_files(&cli, logger_cpy, observation_parquet, forecast_parquet).await?;
    Ok(())
}
