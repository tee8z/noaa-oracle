use std::sync::Arc;

use axum::{extract::State, http::StatusCode};

use crate::AppState;

/// Readiness: the writer accepts commands and the database answers a read.
/// Returns 503 during shutdown so load balancers drain this instance.
pub async fn ready(State(state): State<Arc<AppState>>) -> StatusCode {
    if state.database.is_ready().await {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Liveness: the HTTP server answers. Does not touch SQLite.
pub async fn healthy() -> StatusCode {
    StatusCode::OK
}
