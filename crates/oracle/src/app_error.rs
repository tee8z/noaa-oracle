//! Error type for weather and file handlers. Every variant maps to a fixed
//! status; internal failures are logged and reported as a generic message,
//! validation failures echo their cause.

use crate::{file_access, weather_data};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use log::{error, warn};
use serde_json::json;

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("failed to get weather data")]
    WeatherData(#[from] weather_data::Error),
    #[error("failed to access file data")]
    FileAccess(#[from] file_access::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let client_error = |message: String| (StatusCode::BAD_REQUEST, message);
        let internal = || {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                String::from("internal error"),
            )
        };
        let (status, message) = match &self {
            AppError::InvalidRequest(reason) => client_error(format!("invalid request: {reason}")),
            AppError::WeatherData(weather_data::Error::InvalidStationId(id)) => {
                client_error(format!("invalid station id: {id:?}"))
            }
            AppError::WeatherData(
                weather_data::Error::Query(_)
                | weather_data::Error::TimeFormat(_)
                | weather_data::Error::TimeParse(_)
                | weather_data::Error::FileAccess(_)
                | weather_data::Error::Schema { .. }
                | weather_data::Error::Task(_),
            ) => internal(),
            AppError::FileAccess(file_access::Error::NotFound(_)) => {
                (StatusCode::NOT_FOUND, String::from("file not found"))
            }
            AppError::FileAccess(file_access::Error::InvalidFileName(name)) => {
                client_error(format!("invalid parquet file name: {name:?}"))
            }
            AppError::FileAccess(error @ file_access::Error::UnboundedListing) => {
                client_error(error.to_string())
            }
            AppError::FileAccess(
                file_access::Error::Io(_)
                | file_access::Error::TimeFormat(_)
                | file_access::Error::TimeParse(_),
            ) => internal(),
        };
        if status.is_server_error() {
            error!("request failed: {:#}", anyhow::Error::from(self));
        } else {
            warn!("request rejected: {self:#}");
        }
        (status, Json(json!({ "error": message }))).into_response()
    }
}
