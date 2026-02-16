use anyhow::anyhow;
use axum::serve;
use futures::TryFutureExt;
use log::{error, info};
use oracle::{
    app, build_app_state, create_folder, get_config_info, get_log_level, setup_logger,
    warm_forecast_cache,
};
use std::{net::SocketAddr, str::FromStr, sync::Arc};
use tokio::{net::TcpListener, signal};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = get_config_info();
    let log_level = get_log_level(&cli);

    setup_logger()
        .level(log_level)
        .level_for("duckdb", log_level)
        .level_for("oracle", log_level)
        .level_for("http_response", log_level)
        .level_for("http_request", log_level)
        .apply()?;

    // Get paths using the new helper methods
    let weather_data = cli.weather_dir();
    let event_data = cli.event_db();
    let static_dir = cli.static_dir();
    let private_key = cli.private_key();
    let remote_url = cli.remote_url();
    let host = cli.host();
    let port = cli.port();

    // Create required directories
    create_folder(&weather_data);
    create_folder(&event_data);

    let socket_addr = SocketAddr::from_str(&format!("{}:{}", host, port))
        .map_err(|e| anyhow!("invalid address: {}", e))?;

    let listener = TcpListener::bind(socket_addr)
        .map_err(|e| anyhow!("error binding to socket: {}", e))
        .await?;

    info!("NOAA Oracle starting...");
    info!("  Listen: http://{}", socket_addr);
    info!("  Docs:   http://{}/docs", socket_addr);
    info!("  Weather data: {}", weather_data);
    info!("  Event DB: {}", event_data);
    info!("  Static: {}", static_dir);

    let app_state = build_app_state(
        remote_url,
        static_dir,
        weather_data,
        event_data,
        private_key,
        cli.s3_bucket,
        cli.s3_endpoint,
    )
    .await
    .map_err(|e| {
        error!("error building app: {}", e);
        e
    })?;

    let oracle = app_state.oracle.clone();

    // Spawn background task to pre-warm and periodically refresh the forecast cache
    let cache_state = Arc::new(app_state.clone());
    tokio::spawn(async move {
        // Initial warm-up
        warm_forecast_cache(&cache_state).await;

        // Refresh every 30 minutes (source data arrives hourly, so at most 30 min stale)
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1800));
        interval.tick().await; // skip the first immediate tick (already warmed)
        loop {
            interval.tick().await;
            // Clear old entries before re-warming
            {
                let mut cache = cache_state.forecast_cache.lock().unwrap();
                cache.clear();
            }
            warm_forecast_cache(&cache_state).await;
        }
    });

    let app = app(app_state);

    serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // Checkpoint WAL before exit so Litestream replicates a complete database.
    // This runs after the server stops accepting requests but before the
    // process exits and Litestream receives SIGTERM.
    info!("Checkpointing WAL before shutdown...");
    oracle.checkpoint().await;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
