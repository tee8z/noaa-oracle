use oracle::{Cli, setup_logger};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::load()?;
    let log_level = cli.log_level();

    setup_logger()
        .level(log_level)
        .level_for("duckdb", log_level)
        .level_for("oracle", log_level)
        .level_for("http_response", log_level)
        .level_for("http_request", log_level)
        .apply()?;

    let configuration = cli.configuration()?;
    oracle::run_until_stop(configuration).await
}
