use anyhow::anyhow;
use axum::serve;
use futures::TryFutureExt;
use log::{error, info};
use oracle::{app, build_app_state, create_folder, get_config_info, get_log_level, setup_logger};
use std::{net::SocketAddr, str::FromStr};
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

    let app_state = build_app_state(remote_url, static_dir, weather_data, event_data, private_key)
        .await
        .map_err(|e| {
            error!("error building app: {}", e);
            e
        })?;

    let app = app(app_state);

    serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

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
