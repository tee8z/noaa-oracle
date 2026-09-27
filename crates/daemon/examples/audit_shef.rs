//! Read-only, bounded single-station RR7 collector probe. Prints the complete
//! source evidence manifest; never uploads, signs an outcome, or writes data.
use anyhow::{Result, ensure};
use daemon::{RateLimiter, XmlFetcher, get_coordinates_with_evidence};
use std::{sync::Arc, time::Duration};
use time::OffsetDateTime;

#[tokio::main]
async fn main() -> Result<()> {
    let station = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: audit_shef ICAO_STATION"))?;
    let logger = slog::Logger::root(slog::Discard, slog::o!());
    let fetcher = Arc::new(XmlFetcher::new(
        logger,
        "noaa-oracle-data-audit/1.0",
        Arc::new(tokio::sync::Mutex::new(RateLimiter::new(
            3,
            Duration::from_secs(15),
        ))),
    )?);
    let (mut stations, catalog) = get_coordinates_with_evidence(fetcher.clone()).await?;
    stations.city_data.retain(|id, _| id == &station);
    ensure!(
        stations.city_data.len() == 1,
        "station is not in the configured official catalog"
    );
    let output =
        daemon::shef::collect(fetcher, &stations, &catalog, OffsetDateTime::now_utc(), 3).await;
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
