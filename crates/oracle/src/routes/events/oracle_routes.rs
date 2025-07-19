use crate::{
    oracle, AddEventEntries, AppState, CreateEvent, Event, EventFilter, EventSummary, NostrAuth,
    WeatherEntry,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{ErrorResponse, IntoResponse, Response},
    Json,
};
use log::{error, info};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{borrow::Borrow, sync::Arc};
use tokio::task;
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
pub async fn get_pubkey(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Base64Pubkey>, ErrorResponse> {
    Ok(Json(Base64Pubkey {
        key: state.oracle.public_key(),
    }))
}

#[utoipa::path(
    get,
    path = "/oracle/npub",
    responses(
        (status = OK, description = "Successfully retrieved oracle's nostr npub", body = Pubkey),
    ))]
pub async fn get_npub(State(state): State<Arc<AppState>>) -> Result<Json<Pubkey>, ErrorResponse> {
    Ok(Json(Pubkey {
        key: state.oracle.npub()?,
    }))
}

#[utoipa::path(
    get,
    path = "/oracle/events",
    params(EventFilter),
    responses(
        (status = OK, description = "Successfully retrieved oracle events", body = Vec<Event>),
    ))]
pub async fn list_events(
    State(state): State<Arc<AppState>>,
    Query(filter): Query<EventFilter>,
) -> Result<Json<Vec<EventSummary>>, ErrorResponse> {
    state
        .oracle
        .list_events(filter)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error retrieving event data: {}", e);
            e.into()
        })
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
    ))]
pub async fn create_event(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateEvent>,
) -> Result<Json<Event>, ErrorResponse> {
    state
        .oracle
        .create_event(pubkey, body)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error saving event data: {}", e);
            e.into()
        })
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
) -> Result<Json<Event>, ErrorResponse> {
    state
        .oracle
        .get_event(&event_id)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error event data: {}", e);
            e.into()
        })
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
    ))]
pub async fn add_event_entries(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path(_event_id): Path<Uuid>,
    Json(body): Json<AddEventEntries>,
) -> Result<Json<Vec<WeatherEntry>>, ErrorResponse> {
    state
        .oracle
        .add_event_entries(pubkey, body.event_id, body.entries)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error adding entries to event: {}", e);
            e.into()
        })
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
) -> Result<Json<WeatherEntry>, ErrorResponse> {
    state
        .oracle
        .get_event_entry(&event_id, &entry_id)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error weather entry data: {}", e);
            e.into()
        })
}

#[utoipa::path(
    post,
    path = "/oracle/update",
    responses(
        (status = OK, description = "Successfully kicked off oracle data update"),
        (status = INTERNAL_SERVER_ERROR, description = "Failed to kick off oracle data update"),
    ))]
pub async fn update_data(State(state): State<Arc<AppState>>) -> Result<StatusCode, ErrorResponse> {
    let mut rng = rand::thread_rng();
    let etl_process_id: usize = rng.gen();
    let oracle_cpy = state.oracle.clone();
    // Kick off etl job, note when shutting down we don't do anything to wait for the task to complete at the moment
    task::spawn(async move {
        info!("starting etl process: {}", etl_process_id);
        match oracle_cpy.etl_data(etl_process_id).await {
            Ok(()) => info!("completed etl process: {}", etl_process_id),
            Err(e) => error!("failed etl process: {} {}", etl_process_id, e),
        }
    });
    Ok(StatusCode::OK)
}

impl IntoResponse for oracle::Error {
    fn into_response(self) -> Response {
        let (status, error_message) = match self.borrow() {
            oracle::Error::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            oracle::Error::MinOutcome(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            oracle::Error::EventMaturity(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            oracle::Error::BadEntry(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            oracle::Error::BadEvent(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            _ => (
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
