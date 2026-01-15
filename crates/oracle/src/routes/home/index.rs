use std::sync::Arc;

use axum::{extract::State, response::Html};

use crate::{templates::home_page, AppState};

pub async fn index_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(home_page(&state.remote_url).into_string())
}
