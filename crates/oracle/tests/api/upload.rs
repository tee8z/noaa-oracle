//! Uploaded files decide attested outcomes: only the uploader may publish,
//! only parquet is accepted, and a published file is never replaced.

use crate::helpers::{MockWeatherAccess, TestApp, signed, spawn_app};
use axum::http::{Method, StatusCode};
use std::sync::Arc;

const NAME: &str = "observations_2030-01-01T00:00:00Z.parquet";

fn parquet(content: &[u8]) -> Vec<u8> {
    [b"PAR1".as_slice(), content, b"PAR1".as_slice()].concat()
}

async fn upload(
    test_app: &TestApp,
    name: &str,
    body: Vec<u8>,
    keys: &nostr::key::Keys,
) -> StatusCode {
    let request = signed(Method::POST, &format!("/file/{name}"), body, keys);
    let status = test_app.send(request).await.0;
    test_app.state.wait_for_etl().await;
    status
}

#[tokio::test]
async fn uploader_publishes_parquet_into_the_dated_directory() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    let body = parquet(b"rows");
    assert_eq!(
        upload(&test_app, NAME, body.clone(), &test_app.uploader).await,
        StatusCode::CREATED
    );
    let stored = test_app.weather_dir.join("2030-01-01").join(NAME);
    assert_eq!(std::fs::read(&stored).unwrap(), body);
    let leftovers: Vec<_> = std::fs::read_dir(stored.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files are cleaned up");
}

#[tokio::test]
async fn published_files_are_never_replaced() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    let original = parquet(b"original");
    assert_eq!(
        upload(&test_app, NAME, original.clone(), &test_app.uploader).await,
        StatusCode::CREATED
    );
    assert_eq!(
        upload(&test_app, NAME, parquet(b"forged"), &test_app.uploader).await,
        StatusCode::CONFLICT
    );
    let stored = test_app.weather_dir.join("2030-01-01").join(NAME);
    assert_eq!(std::fs::read(stored).unwrap(), original);
}

#[tokio::test]
async fn invalid_uploads_are_rejected() {
    let test_app = spawn_app(Arc::new(MockWeatherAccess::new())).await;
    assert_eq!(
        upload(&test_app, NAME, b"not parquet".to_vec(), &test_app.uploader).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        upload(
            &test_app,
            "notes_2030-01-01T00:00:00Z.parquet",
            parquet(b"x"),
            &test_app.uploader
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        upload(&test_app, NAME, parquet(b"x"), &test_app.coordinator).await,
        StatusCode::FORBIDDEN
    );
    assert!(!test_app.weather_dir.join("2030-01-01").join(NAME).exists());
}
