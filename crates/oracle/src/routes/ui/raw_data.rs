use std::sync::Arc;

use axum::{extract::State, response::Html};

use crate::{templates::raw_data_page, AppState};

/// Handler for the raw data page (GET /raw)
pub async fn raw_data_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(raw_data_page(&state.remote_url).into_string())
}
