use crate::{AppError, AppState, file_access::FileParams};
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
    Query(params): Query<FileParams>,
) -> Result<Json<Files>, AppError> {
    let file_names = state.file_access.grab_file_names(params).await?;
    Ok(Json(Files { file_names }))
}
