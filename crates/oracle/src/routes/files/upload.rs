use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
};
use log::{error, info};
use std::sync::Arc;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::{fs::File, io::AsyncWriteExt};

use crate::AppState;
use noaa_oracle_core::fs::create_dir_all;

#[utoipa::path(
    post,
    path = "file/{file_name}",
    params(
         ("file_name" = String, Path, description = "Name of file to upload"),
    ),
    responses(
        (status = OK, description = "Successfully uploaded weather data file"),
        (status = BAD_REQUEST, description = "Invalid file"),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to save file")
    ))]
pub async fn upload(
    State(state): State<Arc<AppState>>,
    Path(file_name): Path<String>,
    mut multipart: Multipart,
) -> Result<(), (StatusCode, String)> {
    if !path_is_valid(&file_name) {
        return Err((StatusCode::BAD_REQUEST, "Invalid file".to_owned()));
    }
    while let Some(field) = multipart.next_field().await.unwrap() {
        let data = field.bytes().await.map_err(|err| {
            error!("error getting file's bytes: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to get file's bytes: {}", err),
            )
        })?;

        info!(
            "length of `{}` is {} mb",
            file_name,
            bytes_to_mb(data.len())
        );

        // Parse the date from the filename to save in the correct date directory
        // Filename format: observations_2026-01-21T23:59:43.269662415Z.parquet
        let file_generated_at = parse_file_timestamp(&file_name).map_err(|err| {
            error!("error parsing timestamp from filename: {}", err);
            (
                StatusCode::BAD_REQUEST,
                format!("Failed to parse timestamp from filename: {}", err),
            )
        })?;

        // Use build_file_path which uses file_generated_at.date() for the directory
        let path = state
            .file_access
            .build_file_path(&file_name, file_generated_at);

        // Ensure the date directory exists
        if let Some(parent) = std::path::Path::new(&path).parent() {
            create_dir_all(parent.to_str().unwrap_or_default()).map_err(|err| {
                error!("error creating directory: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create directory: {}", err),
                )
            })?;
        }

        // Create a new file and write the data to it
        let mut file = File::create(&path).await.map_err(|err| {
            error!("error creating file: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create file: {}", err),
            )
        })?;
        file.write_all(&data).await.map_err(|err| {
            error!("error writing file: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write to file: {}", err),
            )
        })?;
    }

    Ok(())
}

/// Parse the timestamp from a filename like "observations_2026-01-21T23:59:43.269662415Z.parquet"
fn parse_file_timestamp(file_name: &str) -> Result<OffsetDateTime, String> {
    let parts: Vec<&str> = file_name.split('_').collect();
    if parts.len() < 2 {
        return Err("Invalid filename format: missing underscore".to_owned());
    }

    let timestamp_str = parts
        .last()
        .ok_or("Invalid filename format")?
        .strip_suffix(".parquet")
        .ok_or("Invalid filename format: missing .parquet suffix")?;

    OffsetDateTime::parse(timestamp_str, &Rfc3339)
        .map_err(|e| format!("Failed to parse timestamp '{}': {}", timestamp_str, e))
}

fn bytes_to_mb(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}

// to prevent directory traversal attacks we ensure the path consists of exactly one normal component
fn path_is_valid(path: &str) -> bool {
    let path = std::path::Path::new(path);

    let mut components = path.components().peekable();

    if let Some(first) = components.peek() {
        if !matches!(first, std::path::Component::Normal(_)) {
            return false;
        }
    }

    components.count() == 1 && is_parquet_file(path)
}

fn is_parquet_file(path: &std::path::Path) -> bool {
    if let Some(extenstion) = path.extension() {
        extenstion == "parquet"
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_file_timestamp_observations() {
        let result = parse_file_timestamp("observations_2026-01-21T23:59:43.269662415Z.parquet");
        assert!(result.is_ok());
        let dt = result.unwrap();
        assert_eq!(dt.date().year(), 2026);
        assert_eq!(dt.date().month() as u8, 1);
        assert_eq!(dt.date().day(), 21);
    }

    #[test]
    fn test_parse_file_timestamp_forecasts() {
        let result = parse_file_timestamp("forecasts_2026-01-21T15:59:43.858149618Z.parquet");
        assert!(result.is_ok());
        let dt = result.unwrap();
        assert_eq!(dt.date().day(), 21);
    }

    #[test]
    fn test_parse_file_timestamp_invalid() {
        assert!(parse_file_timestamp("invalid.parquet").is_err());
        assert!(parse_file_timestamp("observations_notadate.parquet").is_err());
    }
}
