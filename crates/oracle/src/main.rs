use anyhow::anyhow;
use axum::serve;
use futures::TryFutureExt;
use log::{error, info};
use oracle::{app, build_app_state, create_folder, get_config_info, get_log_level, setup_logger};
use std::{net::SocketAddr, str::FromStr};
use tokio::{net::TcpListener, signal};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli: oracle::Cli = get_config_info();
    let log_level = get_log_level(&cli);
    setup_logger()
        .level(log_level)
        .level_for("duckdb", log_level)
        .level_for("oracle", log_level)
        .level_for("http_response", log_level)
        .level_for("http_request", log_level)
        .apply()?;
    let weather_data = cli.weather_dir.unwrap_or(String::from("./weather_data"));
    create_folder(&weather_data.clone());
    let event_data = cli.event_db.unwrap_or(String::from("./event_data"));
    create_folder(&event_data.clone());
    let socket_addr = SocketAddr::from_str(&format!(
        "{}:{}",
        cli.domain.unwrap_or(String::from("127.0.0.1")),
        cli.port.unwrap_or(String::from("9800"))
    ))
    .unwrap();

    let listener = TcpListener::bind(socket_addr)
        .map_err(|e| anyhow!("error binding to IO socket: {}", e.to_string()))
        .await?;

    info!("listening on http://{}", socket_addr);
    info!("docs hosted @ http://{}/docs", socket_addr);

    let app_state = build_app_state(
        cli.remote_url
            .unwrap_or(String::from("http://127.0.0.1:9100")),
        cli.ui_dir.unwrap_or(String::from("./ui")),
        weather_data,
        event_data,
        cli.oracle_private_key
            .unwrap_or(String::from("./oracle_private_key.pem")),
    )
    .await
    .map_err(|e| {
        error!("error building app: {}", e);
        e
    })?;

    let app = app(app_state.clone());

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
