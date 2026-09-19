use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use log::{error, info, warn};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState, EtlRejected,
    auth::{Role, Signed},
    database::WriteError,
    events::{
        AddEventEntries, CreateEvent, EntryRejection, Event, EventFilter, EventSummary,
        WeatherEntry,
    },
    oracle::Error,
    sources::Metric,
};

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Base64Pubkey {
    /// base64 of the compressed SEC1 public key: a parity byte (`0x02` or
    /// `0x03`) followed by the big-endian X coordinate.
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Pubkey {
    /// nostr npub in string format
    pub key: String,
}

/// Error body for every oracle route.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: String,
}

#[utoipa::path(
    get,
    path = "/oracle/pubkey",
    responses(
        (status = OK, description = "The oracle's attestation public key", body = Base64Pubkey),
    ))]
pub async fn get_pubkey(State(state): State<Arc<AppState>>) -> Json<Base64Pubkey> {
    Json(Base64Pubkey {
        key: state.oracle.public_key_base64(),
    })
}

#[utoipa::path(
    get,
    path = "/oracle/npub",
    responses(
        (status = OK, description = "The oracle's nostr npub (the same key)", body = Pubkey),
    ))]
pub async fn get_npub(State(state): State<Arc<AppState>>) -> Json<Pubkey> {
    Json(Pubkey {
        key: state.oracle.npub(),
    })
}

/// A data source events can attest to.
#[derive(Debug, Serialize, ToSchema)]
pub struct SourceInfo {
    /// Value for `CreateEvent.source`
    pub id: &'static str,
    /// Metric ids usable in `scoring_fields` and picks, with their par rules
    pub metrics: Vec<Metric>,
    /// Metrics scored when an event names none
    pub default_metrics: Vec<&'static str>,
    /// Whether events without a `source` use this one
    pub default: bool,
}

#[utoipa::path(
    get,
    path = "/oracle/sources",
    responses(
        (status = OK, description = "Data sources and the metrics each can score", body = Vec<SourceInfo>),
    ))]
pub async fn list_sources(State(state): State<Arc<AppState>>) -> Json<Vec<SourceInfo>> {
    let sources = state.oracle.sources();
    let default = sources.default_source().id();
    Json(
        sources
            .all()
            .map(|source| SourceInfo {
                id: source.id().as_str(),
                metrics: source.metrics().to_vec(),
                default_metrics: source.default_metrics(),
                default: source.id() == default,
            })
            .collect(),
    )
}

#[utoipa::path(
    get,
    path = "/oracle/events",
    params(EventFilter),
    responses(
        (status = OK, description = "Newest events first, at most 100", body = Vec<EventSummary>),
        (status = BAD_REQUEST, description = "Invalid event id in the filter", body = ErrorBody),
    ))]
pub async fn list_events(
    State(state): State<Arc<AppState>>,
    Query(filter): Query<EventFilter>,
) -> Result<Json<Vec<EventSummary>>, Error> {
    state.oracle.list_events(filter).await.map(Json)
}

#[utoipa::path(
    post,
    path = "/oracle/events",
    request_body = CreateEvent,
    responses(
        (status = OK, description = "Created event with its announcement", body = Event),
        (status = BAD_REQUEST, description = "Invalid event", body = ErrorBody),
        (status = UNAUTHORIZED, description = "Missing, stale, replayed, or mismatched NIP-98 authorization"),
        (status = FORBIDDEN, description = "Signer is not an allowed coordinator"),
        (status = SERVICE_UNAVAILABLE, description = "The oracle is shutting down or its write queue is full; retry later", body = ErrorBody),
    ))]
pub async fn create_event(
    State(state): State<Arc<AppState>>,
    signed: Signed,
) -> Result<Json<Event>, Response> {
    state
        .auth
        .require(Role::Coordinator, &signed.pubkey)
        .map_err(IntoResponse::into_response)?;
    let event: CreateEvent = json_body(&signed.body)?;
    state
        .oracle
        .create_event(signed.pubkey, event)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}

#[utoipa::path(
    get,
    path = "/oracle/events/{event_id}",
    params(
        ("event_id" = Uuid, Path, description = "ID of a weather event the oracle is tracking"),
    ),
    responses(
        (status = OK, description = "Event with entries, readings, and announcement", body = Event),
        (status = NOT_FOUND, description = "Event not found", body = ErrorBody),
    ))]
pub async fn get_event(
    State(state): State<Arc<AppState>>,
    Path(event_id): Path<Uuid>,
) -> Result<Json<Event>, Error> {
    state.oracle.get_event(event_id).await.map(Json)
}

#[utoipa::path(
    post,
    path = "/oracle/events/{event_id}/entries",
    request_body = AddEventEntries,
    responses(
        (status = OK, description = "Stored entries", body = Vec<WeatherEntry>),
        (status = BAD_REQUEST, description = "Invalid entries", body = ErrorBody),
        (status = UNAUTHORIZED, description = "Missing, stale, replayed, or mismatched NIP-98 authorization"),
        (status = FORBIDDEN, description = "Signer is not this event's coordinator"),
        (status = CONFLICT, description = "Entries were already submitted", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "The oracle is shutting down or its write queue is full; retry later", body = ErrorBody),
    ))]
pub async fn add_event_entries(
    State(state): State<Arc<AppState>>,
    Path(event_id): Path<Uuid>,
    signed: Signed,
) -> Result<Json<Vec<WeatherEntry>>, Response> {
    state
        .auth
        .require(Role::Coordinator, &signed.pubkey)
        .map_err(IntoResponse::into_response)?;
    let body: AddEventEntries = json_body(&signed.body)?;
    if body.event_id != event_id {
        return Err(bad_request(format!(
            "entries are for event {} but were posted to event {event_id}",
            body.event_id
        )));
    }
    state
        .oracle
        .add_event_entries(signed.pubkey, event_id, body.entries)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}

#[utoipa::path(
    get,
    path = "/oracle/events/{event_id}/entries/{entry_id}",
    params(
        ("event_id" = Uuid, Path, description = "ID of a weather event the oracle is tracking"),
        ("entry_id" = Uuid, Path, description = "ID of an entry in that event"),
    ),
    responses(
        (status = OK, description = "The entry", body = WeatherEntry),
        (status = NOT_FOUND, description = "Entry not found", body = ErrorBody),
    ))]
pub async fn get_event_entry(
    State(state): State<Arc<AppState>>,
    Path((event_id, entry_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<WeatherEntry>, Error> {
    state
        .oracle
        .get_event_entry(event_id, entry_id)
        .await
        .map(Json)
}

#[utoipa::path(
    post,
    path = "/oracle/update",
    responses(
        (status = ACCEPTED, description = "Processing started in the background"),
        (status = UNAUTHORIZED, description = "Missing or invalid NIP-98 authorization"),
        (status = FORBIDDEN, description = "Signer is not an allowed uploader"),
        (status = CONFLICT, description = "Processing is already running"),
        (status = SERVICE_UNAVAILABLE, description = "The oracle is shutting down"),
    ))]
pub async fn update_data(State(state): State<Arc<AppState>>, signed: Signed) -> Response {
    if let Err(error) = state.auth.require(Role::Uploader, &signed.pubkey) {
        return error.into_response();
    }
    match state.start_etl() {
        Ok(etl_process_id) => {
            info!("accepted etl process: {etl_process_id}");
            StatusCode::ACCEPTED.into_response()
        }
        Err(EtlRejected::AlreadyRunning) => StatusCode::CONFLICT.into_response(),
        Err(EtlRejected::ShuttingDown) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn json_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|error| bad_request(format!("invalid JSON body: {error}")))
}

fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Error::EventNotFound(_) | Error::EntryNotFound { .. } => StatusCode::NOT_FOUND,
            Error::InvalidEvent(_) | Error::InvalidFilter(_) => StatusCode::BAD_REQUEST,
            Error::InvalidEntries(EntryRejection::AlreadySubmitted) => StatusCode::CONFLICT,
            Error::InvalidEntries(_) => StatusCode::BAD_REQUEST,
            Error::WrongCoordinator(_) => StatusCode::FORBIDDEN,
            // Rejected before admission: nothing was written, so the caller
            // can retry once the queue drains or a new instance is ready.
            Error::Write(WriteError::Unavailable) => StatusCode::SERVICE_UNAVAILABLE,
            Error::Write(WriteError::OutcomeUnknown | WriteError::Database(_))
            | Error::UnknownSource { .. }
            | Error::Key(_)
            | Error::KeyMismatch { .. }
            | Error::Read(_)
            | Error::Source(_)
            | Error::Attest { .. }
            | Error::Score(_)
            | Error::Task(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = match &self {
            Error::Write(WriteError::Unavailable) => {
                String::from("oracle is not accepting writes right now; retry later")
            }
            // Admitted but the reply was lost: the write may have committed.
            Error::Write(WriteError::OutcomeUnknown) => {
                String::from("write outcome unknown; check the event before retrying")
            }
            _ if status == StatusCode::INTERNAL_SERVER_ERROR => {
                String::from("internal server error")
            }
            _ => self.to_string(),
        };
        if status.is_server_error() {
            error!("oracle request failed: {:#}", anyhow::Error::from(self));
        } else {
            warn!("oracle request rejected: {self}");
        }
        (status, Json(json!({ "error": message }))).into_response()
    }
}
