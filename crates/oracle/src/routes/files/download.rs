use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, header},
};
use std::sync::Arc;

use crate::{AppError, AppState, file_access::ParquetFileName};

#[utoipa::path(
    get,
    path = "/file/{filename}",
    params(
         ("filename" = String, Path, description = "Name of file to download"),
    ),
    responses(
        (status = OK, description = "Successfully retrieved file", content_type = "application/parquet", body = Vec<u8>),
        (status = BAD_REQUEST, description = "Invalid file name"),
        (status = NOT_FOUND, description = "File not found"),
    ))]
pub async fn download(
    State(state): State<Arc<AppState>>,
    Path(filename): Path<String>,
) -> Result<(HeaderMap, Body), AppError> {
    let file = ParquetFileName::parse(&filename)?;
    let body = state.file_access.download_file(&file).await?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/parquet"),
    );
    // The validated name contains only ASCII, so the header value is valid.
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{file}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    Ok((headers, body))
}
