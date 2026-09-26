//! Data upload from the daemon. The uploaded files decide attested
//! outcomes, so only allowlisted uploaders may publish, files are never
//! replaced once published, and a file becomes visible to queries only
//! after it is completely written.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use log::{error, info};
use serde_json::json;
use std::{io, path::Path as FsPath, sync::Arc};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

use crate::{
    AppState,
    auth::{Role, Signed},
    file_access::ParquetFileName,
};

/// Every parquet file starts and ends with this magic.
const PARQUET_MAGIC: &[u8] = b"PAR1";

#[utoipa::path(
    post,
    path = "/file/{file_name}",
    params(
         ("file_name" = String, Path, description = "`observations_<rfc3339>.parquet` or `forecasts_<rfc3339>.parquet`"),
    ),
    request_body(content = Vec<u8>, content_type = "application/vnd.apache.parquet"),
    responses(
        (status = CREATED, description = "Stored the file and started processing"),
        (status = BAD_REQUEST, description = "Invalid file name or not a parquet file"),
        (status = UNAUTHORIZED, description = "Missing or invalid NIP-98 authorization"),
        (status = FORBIDDEN, description = "Signer is not an allowed uploader"),
        (status = CONFLICT, description = "A file with this name was already published"),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to store the file"),
    ))]
pub async fn upload(
    State(state): State<Arc<AppState>>,
    Path(file_name): Path<String>,
    signed: Signed,
) -> Response {
    if let Err(error) = state.auth.require(Role::Uploader, &signed.pubkey) {
        return error.into_response();
    }
    // Only the daemon's naming scheme is accepted; the name selects the
    // date directory and is later interpolated into DuckDB queries.
    let file = match ParquetFileName::parse(&file_name) {
        Ok(file) => file,
        Err(error) => return reject(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let body = &signed.body;
    if body.len() < 2 * PARQUET_MAGIC.len()
        || !body.starts_with(PARQUET_MAGIC)
        || !body.ends_with(PARQUET_MAGIC)
    {
        return reject(StatusCode::BAD_REQUEST, "body is not a parquet file");
    }
    let directory = state.weather_dir.join(file.generated_at.date().to_string());
    let target = directory.join(file.to_string());
    match publish(&directory, &target, body).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return reject(StatusCode::CONFLICT, "file was already published");
        }
        Err(error) => {
            error!("failed to store {}: {error}", target.display());
            return reject(StatusCode::INTERNAL_SERVER_ERROR, "failed to store file");
        }
    }
    info!("stored {file} ({:.3} MiB)", body.len() as f64 / 1_048_576.0);
    state.metrics().upload_accepted(file.kind);
    state.file_added();
    state.new_data();
    // New data may settle events; a pass already running will see it next time.
    if let Err(rejected) = state.start_etl() {
        info!("not starting processing after upload: {rejected:?}");
    }
    StatusCode::CREATED.into_response()
}

/// Writes `body` to a temporary file beside `target`, syncs it, and links
/// it into place. Fails with `AlreadyExists` instead of replacing a file.
async fn publish(directory: &FsPath, target: &FsPath, body: &[u8]) -> io::Result<()> {
    fs::create_dir_all(directory).await?;
    if fs::try_exists(target).await? {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let temporary = directory.join(format!(".upload-{}.tmp", Uuid::now_v7()));
    let result = async {
        let mut file = fs::File::create_new(&temporary).await?;
        file.write_all(body).await?;
        file.sync_all().await?;
        // `hard_link` fails if the target exists, so a concurrent upload of
        // the same name cannot replace a published file.
        fs::hard_link(&temporary, target).await
    }
    .await;
    let _ = fs::remove_file(&temporary).await;
    result
}

fn reject(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
