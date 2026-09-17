use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
};
use log::{error, info};
use std::sync::Arc;
use tokio::{fs::File, io::AsyncWriteExt};

use crate::{AppState, file_access::ParquetFileName};

#[utoipa::path(
    post,
    path = "/file/{file_name}",
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
    // Only the daemon's naming scheme is accepted; the name selects the
    // date directory and is later interpolated into DuckDB queries.
    let file = ParquetFileName::parse(&file_name)
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    while let Some(field) = multipart.next_field().await.map_err(|err| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid multipart body: {err}"),
        )
    })? {
        let data = field.bytes().await.map_err(|err| {
            error!("error getting file's bytes: {}", err);
            (
                StatusCode::BAD_REQUEST,
                format!("Failed to get file's bytes: {}", err),
            )
        })?;

        info!("length of `{}` is {:.3} mb", file, bytes_to_mb(data.len()));

        let path = state.file_access.build_file_path(&file);
        if let Some(parent) = std::path::Path::new(&path).parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|err| {
                error!("error creating directory: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create directory: {}", err),
                )
            })?;
        }

        let mut output = File::create(&path).await.map_err(|err| {
            error!("error creating file: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create file: {}", err),
            )
        })?;
        output.write_all(&data).await.map_err(|err| {
            error!("error writing file: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write to file: {}", err),
            )
        })?;
    }

    Ok(())
}

fn bytes_to_mb(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}
