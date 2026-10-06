//! Error type for weather and file handlers. Every variant maps to a fixed
//! status; internal failures are logged and reported as a generic message,
//! validation failures echo their cause.

use crate::{file_access, heavy::RETRY_AFTER_SECONDS, weather_data};
use axum::{
    Json,
    http::{StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
};
use log::{error, warn};
use serde_json::json;
use std::sync::Arc;

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("failed to get weather data")]
    WeatherData(#[from] weather_data::Error),
    #[error("failed to access file data")]
    FileAccess(#[from] file_access::Error),
    /// Heavy work had no turn, or was not done within the request's time
    /// (see [`crate::heavy`]). Answered 503 with `Retry-After`.
    #[error("busy: {0}")]
    Busy(&'static str),
    /// The outcome of one build, shared by every request that waited for it.
    #[error(transparent)]
    Shared(Arc<AppError>),
}

impl AppError {
    /// The error itself, through any sharing.
    fn cause(&self) -> &AppError {
        match self {
            AppError::Shared(shared) => shared.cause(),
            error => error,
        }
    }

    fn status_and_message(&self) -> (StatusCode, String) {
        let client_error = |message: String| (StatusCode::BAD_REQUEST, message);
        let internal = || {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                String::from("internal error"),
            )
        };
        match self.cause() {
            AppError::InvalidRequest(reason) => client_error(format!("invalid request: {reason}")),
            AppError::Busy(reason) => (StatusCode::SERVICE_UNAVAILABLE, (*reason).to_owned()),
            AppError::Shared(_) => internal(),
            AppError::WeatherData(weather_data::Error::InvalidStationId(id)) => {
                client_error(format!("invalid station id: {id:?}"))
            }
            AppError::WeatherData(
                error @ (weather_data::Error::DataQuality { .. }
                | weather_data::Error::ObservationCoverage { .. }
                | weather_data::Error::ForecastQuality { .. }
                | weather_data::Error::QualityUnavailable
                | weather_data::Error::LegacyObservations { .. }),
            ) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()),
            AppError::WeatherData(
                weather_data::Error::Query(_)
                | weather_data::Error::TimeFormat(_)
                | weather_data::Error::TimeParse(_)
                | weather_data::Error::FileAccess(_)
                | weather_data::Error::Schema { .. }
                | weather_data::Error::Task(_)
                | weather_data::Error::Io(_),
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
        }
    }

    /// Whether the request was turned away for want of a turn or time.
    pub fn is_busy(&self) -> bool {
        matches!(self.cause(), AppError::Busy(_))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = self.status_and_message();
        let body = Json(json!({ "error": message }));
        if let AppError::Busy(reason) = self.cause() {
            warn!("request turned away: {reason}");
            return (
                status,
                [(RETRY_AFTER, RETRY_AFTER_SECONDS.to_string())],
                body,
            )
                .into_response();
        }
        if status.is_server_error() {
            error!("request failed: {:#}", anyhow::Error::from(self));
        } else {
            warn!("request rejected: {self:#}");
        }
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_failures_answer_like_their_cause() {
        let invalid = AppError::Shared(Arc::new(AppError::InvalidRequest("days".into())));
        assert_eq!(
            invalid.status_and_message(),
            (StatusCode::BAD_REQUEST, "invalid request: days".into())
        );
        let busy = AppError::Shared(Arc::new(AppError::Busy("busy")));
        assert!(busy.is_busy());
        let response = busy.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers()[RETRY_AFTER],
            RETRY_AFTER_SECONDS.to_string()
        );
        let failed = AppError::Shared(Arc::new(AppError::WeatherData(
            weather_data::Error::QualityUnavailable,
        )));
        assert!(!failed.is_busy());
        assert_eq!(
            failed.status_and_message().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
