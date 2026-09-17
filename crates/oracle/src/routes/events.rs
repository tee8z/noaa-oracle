use crate::{
    AppState,
    database::WriteError,
    events::{AddEventEntries, CreateEvent, Event, EventFilter, EventSummary, WeatherEntry},
    nostr_extractor::NostrAuth,
    oracle,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use log::{error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Base64Pubkey {
    /// base64 representation of the compressed DER encoding of the publickey. This consists of a parity
    /// byte at the beginning, which is either `0x02` (even parity) or `0x03` (odd parity),
    /// followed by the big-endian encoding of the point's X-coordinate.
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Pubkey {
    /// nostr npub in string format
    pub key: String,
}

#[utoipa::path(
    get,
    path = "/oracle/pubkey",
    responses(
        (status = OK, description = "Successfully retrieved oracle's pubkey data", body = Base64Pubkey),
    ))]
pub async fn get_pubkey(State(state): State<Arc<AppState>>) -> Json<Base64Pubkey> {
    Json(Base64Pubkey {
        key: state.oracle.public_key(),
    })
}

#[utoipa::path(
    get,
    path = "/oracle/npub",
    responses(
        (status = OK, description = "Successfully retrieved oracle's nostr npub", body = Pubkey),
    ))]
pub async fn get_npub(State(state): State<Arc<AppState>>) -> Result<Json<Pubkey>, oracle::Error> {
    Ok(Json(Pubkey {
        key: state.oracle.npub()?,
    }))
}

#[utoipa::path(
    get,
    path = "/oracle/events",
    params(EventFilter),
    responses(
        (status = OK, description = "Successfully retrieved oracle events", body = Vec<EventSummary>),
    ))]
pub async fn list_events(
    State(state): State<Arc<AppState>>,
    Query(filter): Query<EventFilter>,
) -> Result<Json<Vec<EventSummary>>, oracle::Error> {
    state
        .oracle
        .list_events(filter)
        .await
        .map(Json)
        .inspect_err(|e| error!("error retrieving event data: {}", e))
}

#[utoipa::path(
    post,
    path = "/oracle/events",
    request_body = CreateEvent,
    responses(
        (status = OK, description = "Successfully created oracle weather event", body = Event),
        (status = BAD_REQUEST, description = "Invalid event to be created"),
        (status = FORBIDDEN, description = "Invalid signature from coordinator in nostr authorization header"),
        (status = UNAUTHORIZED, description = "Invalid nostr authorization header nip-98 using coordinator keys"),
        (status = SERVICE_UNAVAILABLE, description = "The oracle is shutting down or its write queue is full; retry later"),
    ))]
pub async fn create_event(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateEvent>,
) -> Result<Json<Event>, oracle::Error> {
    state
        .oracle
        .create_event(pubkey, body)
        .await
        .map(Json)
        .inspect_err(|e| error!("error saving event data: {}", e))
}

#[utoipa::path(
    get,
    path = "/oracle/events/{event_id}",
    params(
        ("event_id" = Uuid, Path, description = "ID of a weather event the oracle is tracking"),
    ),
    responses(
        (status = OK, description = "Successfully retrieved event data", body = Event),
        (status = NOT_FOUND, description = "Event not found for the provided ID"),
    ))]
pub async fn get_event(
    State(state): State<Arc<AppState>>,
    Path(event_id): Path<Uuid>,
) -> Result<Json<Event>, oracle::Error> {
    state
        .oracle
        .get_event(&event_id)
        .await
        .map(Json)
        .inspect_err(|e| error!("error event data: {}", e))
}

#[utoipa::path(
    post,
    path = "/oracle/events/{event_id}/entries",
    request_body = AddEventEntries,
    responses(
        (status = OK, description = "Successfully add entries into oracle weather event", body = Vec<WeatherEntry>),
        (status = BAD_REQUEST, description = "Invalid entries to be created"),
        (status = FORBIDDEN, description = "Invalid signature from coordinator in nostr authorization header"),
        (status = UNAUTHORIZED, description = "Invalid nostr authorization header nip-98 using coordinator keys"),
        (status = SERVICE_UNAVAILABLE, description = "The oracle is shutting down or its write queue is full; retry later"),
    ))]
pub async fn add_event_entries(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path(event_id): Path<Uuid>,
    Json(body): Json<AddEventEntries>,
) -> Result<Json<Vec<WeatherEntry>>, oracle::Error> {
    if body.event_id != event_id {
        return Err(oracle::Error::BadEntry(format!(
            "entries are for event {} but were posted to event {}",
            body.event_id, event_id
        )));
    }
    state
        .oracle
        .add_event_entries(pubkey, body.event_id, body.entries)
        .await
        .map(Json)
        .inspect_err(|e| error!("error adding entries to event: {}", e))
}

#[utoipa::path(
    get,
    path = "/oracle/events/{event_id}/entries/{entry_id}",
    params(
        ("event_id" = Uuid, Path, description = "ID of a weather event the oracle is tracking"),
        ("entry_id" = Uuid, Path, description = "ID of a entry into weather event the oracle is tracking"),
    ),
    responses(
        (status = OK, description = "Successfully retrieved event entry", body = WeatherEntry),
        (status = NOT_FOUND, description = "Event entry not found for the provided ID"),
    ))]
pub async fn get_event_entry(
    State(state): State<Arc<AppState>>,
    Path((event_id, entry_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<WeatherEntry>, oracle::Error> {
    state
        .oracle
        .get_event_entry(&event_id, &entry_id)
        .await
        .map(Json)
        .inspect_err(|e| error!("error weather entry data: {}", e))
}

#[utoipa::path(
    post,
    path = "/oracle/update",
    responses(
        (status = ACCEPTED, description = "Oracle data update started in the background"),
        (status = CONFLICT, description = "An update is already running"),
        (status = SERVICE_UNAVAILABLE, description = "The oracle is shutting down"),
    ))]
pub async fn update_data(State(state): State<Arc<AppState>>) -> StatusCode {
    match state.start_etl() {
        Ok(etl_process_id) => {
            info!("accepted etl process: {}", etl_process_id);
            StatusCode::ACCEPTED
        }
        Err(crate::EtlRejected::AlreadyRunning) => StatusCode::CONFLICT,
        Err(crate::EtlRejected::ShuttingDown) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

impl IntoResponse for oracle::Error {
    fn into_response(self) -> Response {
        let (status, error_message) = match &self {
            oracle::Error::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            oracle::Error::BadEntry(_) | oracle::Error::BadEvent(_) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            oracle::Error::WeatherData(error) => match error {
                crate::weather_data::Error::InvalidStationId(_) => {
                    (StatusCode::BAD_REQUEST, self.to_string())
                }
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    String::from("internal server error"),
                ),
            },
            // Rejected before admission: nothing was written, so the caller
            // can retry once the queue drains or a new instance is ready.
            oracle::Error::Write(WriteError::Unavailable) => (
                StatusCode::SERVICE_UNAVAILABLE,
                String::from("oracle is not accepting writes right now; retry later"),
            ),
            // Admitted but the reply was lost: the write may have committed.
            oracle::Error::Write(WriteError::OutcomeUnknown) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                String::from("write outcome unknown; check the event before retrying"),
            ),
            oracle::Error::Write(WriteError::Database(_))
            | oracle::Error::DataQuery(_)
            | oracle::Error::Key(_)
            | oracle::Error::ConvertKey(_)
            | oracle::Error::MismatchPubkey(_)
            | oracle::Error::OutcomeNotFound(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                String::from("internal server error"),
            ),
        };
        let body = Json(json!({
            "error": error_message,
        }));
        (status, body).into_response()
    }
}
