//! Error type for weather and file handlers. Internal failures are logged
//! and reported as a generic message; validation failures echo their cause.

use crate::{file_access, weather_data};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use log::error;
use serde_json::json;

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("Failed to validate request: {0}")]
    Request(#[from] anyhow::Error),
    #[error("Failed to get weather data: {0}")]
    WeatherData(#[from] weather_data::Error),
    #[error("Failed to access file data: {0}")]
    FileAccess(#[from] file_access::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!("error handling request: {}", self);

        let (status, error_message) = match &self {
            AppError::Request(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            AppError::WeatherData(weather_data::Error::Query(_))
            | AppError::WeatherData(weather_data::Error::FileAccess(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                String::from("internal error"),
            ),
            AppError::WeatherData(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            AppError::FileAccess(file_access::Error::Io(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                String::from("internal error"),
            ),
            AppError::FileAccess(file_access::Error::NotFound(_)) => {
                (StatusCode::NOT_FOUND, self.to_string())
            }
            AppError::FileAccess(_) => (StatusCode::BAD_REQUEST, self.to_string()),
        };

        (status, Json(json!({ "error": error_message }))).into_response()
    }
}
