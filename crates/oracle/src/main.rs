use oracle::{Cli, setup_logger};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::load()?;
    let log_level = cli.log_level();

    // Held until main returns, so queued log lines are written at exit.
    let (logger, _log_guard) = setup_logger();
    logger
        .level(log_level)
        .level_for("duckdb", log_level)
        .level_for("oracle", log_level)
        .level_for("http_response", log_level)
        .level_for("http_request", log_level)
        .apply()?;

    let configuration = cli.configuration()?;
    oracle::run_until_stop(configuration).await
}
