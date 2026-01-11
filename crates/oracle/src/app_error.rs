use crate::{file_access, weather_data};
use axum::{
    response::{IntoResponse, Response},
    Json,
};
use hyper::StatusCode;
use log::error;
use serde_json::json;
use std::borrow::Borrow;

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("Failed to validate request: {0}")]
    Request(#[from] anyhow::Error),
    #[error("Failed to get weather data: {0}")]
    WeatherData(#[from] weather_data::Error),
    #[error("Failed to parse times for file data: {0}")]
    FileAccess(#[from] file_access::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!("error handling request: {}", self);

        let (status, error_message) = match self.borrow() {
            AppError::Request(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            AppError::WeatherData(e) => match e {
                weather_data::Error::Query(_) | &weather_data::Error::FileAccess(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    String::from("internal error"),
                ),
                _ => (StatusCode::BAD_REQUEST, self.to_string()),
            },
            AppError::FileAccess(_) => (StatusCode::BAD_REQUEST, self.to_string()),
        };

        let body = Json(json!({
            "error": error_message,
        }));
        (status, body).into_response()
    }
}
