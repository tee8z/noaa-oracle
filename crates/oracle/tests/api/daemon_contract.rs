//! The daemon's publisher, as shipped in the `daemon` crate, is accepted by
//! the oracle over real HTTP: signature, URL, payload hash, file name, and
//! status handling all line up. A mismatch here means no data reaches the
//! oracle in production.

use crate::helpers::{MockWeatherAccess, spawn_app_at};
use daemon::{
    keys::npub,
    publish::{Artifact, PublishError, Publisher},
};
use nostr::{key::Keys, types::Url};
use std::sync::Arc;

fn parquet(content: &[u8]) -> Vec<u8> {
    [b"PAR1".as_slice(), content, b"PAR1".as_slice()].concat()
}

async fn next_second() {
    let start = time::OffsetDateTime::now_utc().unix_timestamp();
    while time::OffsetDateTime::now_utc().unix_timestamp() == start {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn logger() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

#[tokio::test]
async fn the_daemon_publisher_is_accepted_over_http() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let test_app = spawn_app_at(Arc::new(MockWeatherAccess::new()), &origin).await;
    let server = tokio::spawn(axum::serve(listener, test_app.app.clone()).into_future());

    // A run's output, staged the way the daemon writes it.
    let staging = tempfile::tempdir().unwrap();
    let day = staging.path().join("2030-01-01");
    std::fs::create_dir_all(&day).unwrap();
    let path = day.join("observations_2030-01-01T00:00:00Z.parquet");
    let body = parquet(b"rows");
    std::fs::write(&path, &body).unwrap();
    let artifact = Artifact::from_path(&path).unwrap();
    let marker = day.join("observations_2030-01-01T00:00:00Z.parquet.uploaded");
    let base_url = Url::parse(&origin).unwrap();

    let publisher =
        Publisher::new(base_url.clone(), test_app.uploader.clone(), None, logger()).unwrap();
    publisher.publish(&artifact).await.unwrap();
    test_app.state.wait_for_etl().await;
    let stored = test_app.weather_dir.join("2030-01-01").join(&artifact.name);
    assert_eq!(std::fs::read(&stored).unwrap(), body);
    assert!(marker.exists(), "a published file is marked uploaded");

    // The oracle keeps its first copy; a retry after a lost reply succeeds.
    // Event ids include `created_at` in whole seconds and the oracle refuses
    // a repeated id as a replay, so wait for the next second, as the real
    // retry on the next run would.
    next_second().await;
    std::fs::remove_file(&marker).unwrap();
    publisher.publish(&artifact).await.unwrap();
    assert!(marker.exists());

    // A key the oracle does not list is refused, and the error names the
    // npub an operator has to allowlist.
    let stranger = Keys::generate();
    let other = day.join("forecasts_2030-01-01T00:00:00Z.parquet");
    std::fs::write(&other, parquet(b"forecast rows")).unwrap();
    let refused = Publisher::new(base_url, stranger.clone(), None, logger())
        .unwrap()
        .publish(&Artifact::from_path(&other).unwrap())
        .await
        .unwrap_err();
    match refused {
        PublishError::Unauthorized { npub: named, .. } => assert_eq!(named, npub(&stranger)),
        error => panic!("expected an authorization error, got {error:?}"),
    }
    assert!(
        !day.join("forecasts_2030-01-01T00:00:00Z.parquet.uploaded")
            .exists(),
        "a refused file is retried next run"
    );
    server.abort();
}
