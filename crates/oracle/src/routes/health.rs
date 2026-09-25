use std::sync::Arc;

use axum::{extract::State, http::StatusCode};

use crate::AppState;

/// Health: the writer accepts commands and the database answers a read.
/// Returns 503 during shutdown so load balancers drain this instance.
pub async fn health(State(state): State<Arc<AppState>>) -> StatusCode {
    if state.database.is_ready().await {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Readiness to take over traffic: [`health`], once the first preparation
/// of recent forecast files for queries has finished. Until then the
/// process answers forecast queries from the published files, which takes
/// seconds per request, so a deploy keeps the previous process serving.
/// Later preparation passes don't affect it.
pub async fn ready(State(state): State<Arc<AppState>>) -> StatusCode {
    if state.files_prepared() {
        health(State(state)).await
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Liveness: the HTTP server answers. Does not touch SQLite.
pub async fn healthy() -> StatusCode {
    StatusCode::OK
}
