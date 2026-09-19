use crate::{
    AppError, AppState,
    file_access::{FileParams, MAX_LIST_DAYS},
};
use time::{Duration, OffsetDateTime};

/// Range listed when a request gives no bounds.
const DEFAULT_LIST_WINDOW: Duration = Duration::days(7);
use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

#[derive(Serialize, Deserialize, ToSchema)]
pub struct Files {
    pub file_names: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/files",
    params(
         FileParams
    ),
    responses(
        (status = OK, description = "Successfully retrieved file names", body = Files),
        (status = BAD_REQUEST, description = "Invalid file params"),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to retrieve file names")
    ))]
pub async fn files(
    State(state): State<Arc<AppState>>,
    Query(mut params): Query<FileParams>,
) -> Result<Json<Files>, AppError> {
    let now = OffsetDateTime::now_utc();
    let end = params.end.unwrap_or(now);
    let start = params.start.unwrap_or(end - DEFAULT_LIST_WINDOW);
    if end < start || end - start > Duration::days(MAX_LIST_DAYS as i64) {
        return Err(AppError::InvalidRequest(format!(
            "start and end must be ordered and at most {MAX_LIST_DAYS} days apart"
        )));
    }
    (params.start, params.end) = (Some(start), Some(end));
    let file_names = state.file_access.grab_file_names(params).await?;
    Ok(Json(Files { file_names }))
}
