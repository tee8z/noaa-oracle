//! NIP-98 request authentication.
//!
//! Writers sign a kind 27235 event over the request method, the full URL,
//! and the SHA-256 of the body. A request is accepted only when:
//!
//! - the signed URL equals the oracle's configured public URL plus the
//!   request path and query (the `Host` header is not trusted),
//! - the method matches and the event is at most [`MAX_EVENT_AGE`] old,
//! - the `payload` tag matches the body (required whenever there is one),
//! - the signer is on an allowlist for some role, and
//! - the event id has not been used before within the freshness window.
//!
//! Handlers then check the specific [`Role`] explicitly.

use axum::{
    Json,
    body::Bytes,
    extract::{FromRequest, OriginalUri, Request},
    http::{Method, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use log::warn;
use nostr::{
    event::{Event, Kind},
    key::PublicKey,
    nips::nip98::{HttpData, HttpMethod},
    types::Url,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};
use time::OffsetDateTime;

use crate::AppState;

/// Accepted clock skew between the signed event and the server.
pub const MAX_EVENT_AGE: Duration = Duration::from_secs(60);
/// An `Authorization` header larger than this is not a NIP-98 event.
const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Remembered event ids. Only allowlisted signers are recorded, so this
/// bounds honest traffic within the freshness window.
const MAX_REMEMBERED_EVENTS: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Creates events and submits entries.
    Coordinator,
    /// Publishes data files and triggers processing.
    Uploader,
}

/// Who may do what, and where requests are signed for.
pub struct AuthPolicy {
    /// Scheme, host, and optional port, without a trailing slash.
    origin: String,
    coordinators: HashSet<PublicKey>,
    uploaders: HashSet<PublicKey>,
    seen: Mutex<HashMap<nostr::event::EventId, Instant>>,
}

impl AuthPolicy {
    pub fn new(
        public_url: &str,
        coordinators: impl IntoIterator<Item = PublicKey>,
        uploaders: impl IntoIterator<Item = PublicKey>,
    ) -> Self {
        Self {
            origin: public_url.trim_end_matches('/').to_owned(),
            coordinators: coordinators.into_iter().collect(),
            uploaders: uploaders.into_iter().collect(),
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn is_known(&self, pubkey: &PublicKey) -> bool {
        self.coordinators.contains(pubkey) || self.uploaders.contains(pubkey)
    }

    pub fn require(&self, role: Role, pubkey: &PublicKey) -> Result<(), AuthError> {
        let allowed = match role {
            Role::Coordinator => &self.coordinators,
            Role::Uploader => &self.uploaders,
        };
        if allowed.contains(pubkey) {
            Ok(())
        } else {
            Err(AuthError::NotAllowed)
        }
    }

    /// Records `id`, failing if it was already used or the cache is full.
    fn remember(&self, id: nostr::event::EventId) -> Result<(), AuthError> {
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        // Twice the age window covers events signed slightly in the future.
        seen.retain(|_, at| now.duration_since(*at) <= 2 * MAX_EVENT_AGE);
        if seen.contains_key(&id) {
            return Err(AuthError::Replayed);
        }
        if seen.len() >= MAX_REMEMBERED_EVENTS {
            return Err(AuthError::Busy);
        }
        seen.insert(id, now);
        Ok(())
    }
}

/// A verified request from an allowlisted signer, with its body.
#[derive(Debug)]
pub struct Signed {
    pub pubkey: PublicKey,
    pub body: Bytes,
}

impl FromRequest<Arc<AppState>> for Signed {
    type Rejection = AuthError;

    async fn from_request(request: Request, state: &Arc<AppState>) -> Result<Self, AuthError> {
        let policy = &state.auth;
        let header = request
            .headers()
            .get(AUTHORIZATION)
            .ok_or(AuthError::Missing)?;
        if header.len() > MAX_HEADER_BYTES {
            return Err(AuthError::Malformed);
        }
        let encoded = header
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Nostr "))
            .ok_or(AuthError::Malformed)?;
        let event: Event = BASE64
            .decode(encoded)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or(AuthError::Malformed)?;
        if event.kind != Kind::HttpAuth || !event.content.is_empty() {
            return Err(AuthError::Malformed);
        }
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let created_at = i64::try_from(event.created_at.as_secs()).unwrap_or(i64::MAX);
        if now.abs_diff(created_at) > MAX_EVENT_AGE.as_secs() {
            return Err(AuthError::Expired);
        }
        let signed =
            HttpData::try_from(event.tags.clone().to_vec()).map_err(|_| AuthError::Malformed)?;
        let path = request
            .extensions()
            .get::<OriginalUri>()
            .map(|OriginalUri(uri)| uri.clone())
            .unwrap_or_else(|| request.uri().clone());
        let path = path.path_and_query().map_or("/", |path| path.as_str());
        let expected_url =
            Url::from_str(&format!("{}{path}", policy.origin)).map_err(|_| AuthError::Mismatch)?;
        let method = http_method(request.method())?;
        if signed.url != expected_url || signed.method != method {
            return Err(AuthError::Mismatch);
        }
        event.verify().map_err(|_| AuthError::BadSignature)?;
        if !policy.is_known(&event.pubkey) {
            return Err(AuthError::NotAllowed);
        }

        let body = Bytes::from_request(request, state)
            .await
            .map_err(|_| AuthError::Body)?;
        let digest: [u8; 32] = Sha256::digest(&body).into();
        let payload_matches = match signed.payload {
            Some(payload) => payload.to_bytes() == digest,
            None => body.is_empty(),
        };
        if !payload_matches {
            return Err(AuthError::PayloadMismatch);
        }
        policy.remember(event.id)?;
        Ok(Self {
            pubkey: event.pubkey,
            body,
        })
    }
}

fn http_method(method: &Method) -> Result<HttpMethod, AuthError> {
    HttpMethod::from_str(method.as_str()).map_err(|_| AuthError::Mismatch)
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing Authorization header")]
    Missing,
    #[error("Authorization is not a NIP-98 event")]
    Malformed,
    #[error("signed event is too old or in the future")]
    Expired,
    #[error("signed URL or method does not match the request")]
    Mismatch,
    #[error("invalid signature")]
    BadSignature,
    #[error("signed payload hash does not match the body")]
    PayloadMismatch,
    #[error("signer is not allowed to do this")]
    NotAllowed,
    #[error("signed event was already used")]
    Replayed,
    #[error("too many recent requests; retry shortly")]
    Busy,
    #[error("request body could not be read")]
    Body,
}

impl AuthError {
    fn code(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Malformed => "malformed",
            Self::Expired => "expired",
            Self::Mismatch => "mismatch",
            Self::BadSignature => "bad_signature",
            Self::PayloadMismatch => "payload_mismatch",
            Self::NotAllowed => "not_allowed",
            Self::Replayed => "replayed",
            Self::Busy => "busy",
            Self::Body => "body",
        }
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        warn!("rejected signed request: {self}");
        let status = match self {
            Self::Missing
            | Self::Expired
            | Self::Mismatch
            | Self::BadSignature
            | Self::Replayed => StatusCode::UNAUTHORIZED,
            Self::NotAllowed => StatusCode::FORBIDDEN,
            Self::Malformed | Self::PayloadMismatch | Self::Body => StatusCode::BAD_REQUEST,
            Self::Busy => StatusCode::SERVICE_UNAVAILABLE,
        };
        let body = json!({ "error": { "type": self.code(), "detail": self.to_string() } });
        (status, Json(body)).into_response()
    }
}
