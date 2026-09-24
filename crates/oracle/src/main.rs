use oracle::{Cli, setup_buffered_logger};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::load()?;
    let log_level = cli.log_level();

    let (logger, _log_guard) = setup_buffered_logger()?;
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
