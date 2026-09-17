//! NIP-98 HTTP authentication extractor.
//!
//! The coordinator signs a kind 27235 event over the request method and URL
//! (and a payload hash for bodies). The extractor verifies the signature,
//! the freshness window, and that the signed URL matches the request URL as
//! seen behind the proxy.

use axum::{
    Json,
    extract::{FromRequestParts, OriginalUri},
    http::{StatusCode, header::AUTHORIZATION, request::Parts},
    response::IntoResponse,
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use log::{debug, warn};
use nostr::{
    event::{Event, Kind},
    key::PublicKey,
    nips::nip98::{HttpData, HttpMethod},
    types::Url,
};
use serde::{Serialize, Serializer, ser::SerializeStruct};
use serde_json::json;
use std::str::FromStr;
use time::OffsetDateTime;

/// Accepted clock skew between the signed event and the server, in seconds.
const MAX_EVENT_AGE_SECONDS: i64 = 60;

#[derive(Clone, Debug)]
pub struct NostrAuth {
    pub pubkey: PublicKey,
    pub event: Event,
    pub http_data: HttpData,
}

impl<S> FromRequestParts<S> for NostrAuth
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .ok_or(AuthError::NoAuthHeader)?;

        let original_uri = parts
            .extensions
            .get::<OriginalUri>()
            .map(|OriginalUri(uri)| uri.clone())
            .unwrap_or_else(|| parts.uri.clone());

        let event_json = auth_header
            .strip_prefix("Nostr ")
            .ok_or(AuthError::InvalidAuthFormat)?;

        let event_bytes = BASE64
            .decode(event_json)
            .map_err(|e| AuthError::InvalidBase64(e.to_string()))?;

        let event: Event = serde_json::from_slice(&event_bytes)
            .map_err(|e| AuthError::InvalidEventJson(e.to_string()))?;

        if event.kind != Kind::HttpAuth {
            return Err(AuthError::InvalidEventKind);
        }

        let now = OffsetDateTime::now_utc().unix_timestamp();
        let created_at = i64::try_from(event.created_at.as_secs()).unwrap_or(i64::MAX);
        if (now - created_at).abs() > MAX_EVENT_AGE_SECONDS {
            return Err(AuthError::ExpiredTimestamp);
        }

        let http_data = HttpData::try_from(event.tags.clone().to_vec())
            .map_err(|e| AuthError::InvalidHttpData(e.to_string()))?;
        let scheme = if parts.headers.contains_key("x-forwarded-proto") {
            "https"
        } else {
            "http"
        };
        let host = parts
            .headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let reconstructed_url = format!("{scheme}://{host}{original_uri}");
        debug!(
            "nip-98 request {} {} signed for {} {}",
            parts.method, reconstructed_url, http_data.method, http_data.url
        );
        let method = HttpMethod::from_str(parts.method.as_str())
            .map_err(|e| AuthError::InvalidMethod(e.to_string()))?;
        if http_data.url != Url::from_str(&reconstructed_url)? || http_data.method != method {
            return Err(AuthError::UrlMethodMismatch);
        }

        if !event.content.is_empty() {
            return Err(AuthError::NonEmptyContent);
        }

        event
            .verify()
            .map_err(|e| AuthError::InvalidSignature(e.to_string()))?;

        Ok(Self {
            pubkey: event.pubkey,
            event,
            http_data,
        })
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AuthError {
    #[error("No authorization header found")]
    NoAuthHeader,
    #[error("Invalid authorization format")]
    InvalidAuthFormat,
    #[error("Invalid base64 encoding: {0}")]
    InvalidBase64(String),
    #[error("Invalid event JSON: {0}")]
    InvalidEventJson(String),
    #[error("Invalid event kind")]
    InvalidEventKind,
    #[error("Event timestamp expired")]
    ExpiredTimestamp,
    #[error("Invalid HTTP data: {0}")]
    InvalidHttpData(String),
    #[error("URL or method mismatch")]
    UrlMethodMismatch,
    #[error("Invalid URL format: {0}")]
    InvalidUrl(String),
    #[error("Invalid method format: {0}")]
    InvalidMethod(String),
    #[error("Invalid signature: {0}")]
    InvalidSignature(String),
    #[error("Event content must be empty")]
    NonEmptyContent,
}

impl From<nostr::types::ParseError> for AuthError {
    fn from(err: nostr::types::ParseError) -> Self {
        AuthError::InvalidUrl(err.to_string())
    }
}

impl Serialize for AuthError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("AuthError", 2)?;

        let type_str = match self {
            Self::NoAuthHeader => "no_auth_header",
            Self::InvalidAuthFormat => "invalid_auth_format",
            Self::InvalidBase64(_) => "invalid_base_64",
            Self::InvalidEventJson(_) => "invalid_event_json",
            Self::InvalidEventKind => "invalid_event_kind",
            Self::InvalidUrl(_) => "invalid_url",
            Self::InvalidMethod(_) => "invalid_method",
            Self::ExpiredTimestamp => "expired_timestamp",
            Self::InvalidHttpData(_) => "invalid_http_data",
            Self::UrlMethodMismatch => "url_method_mismatch",
            Self::InvalidSignature(_) => "invalid_signature",
            Self::NonEmptyContent => "non_empty_content",
        };

        state.serialize_field("type", type_str)?;
        state.serialize_field("detail", &self.to_string())?;
        state.end()
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> axum::response::Response {
        warn!("{}", self);
        let code = match &self {
            Self::InvalidSignature(_) => StatusCode::FORBIDDEN,
            Self::NoAuthHeader
            | Self::InvalidEventKind
            | Self::ExpiredTimestamp
            | Self::UrlMethodMismatch
            | Self::InvalidUrl(_)
            | Self::InvalidMethod(_) => StatusCode::UNAUTHORIZED,
            Self::InvalidAuthFormat
            | Self::InvalidBase64(_)
            | Self::InvalidEventJson(_)
            | Self::InvalidHttpData(_)
            | Self::NonEmptyContent => StatusCode::BAD_REQUEST,
        };
        (code, Json(json!({ "error": self }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use nostr::{
        event::{EventBuilder, FinalizeEvent, IntoEventBuilder, Tag},
        key::Keys,
        nips::nip98::Sha256Hash,
        types::Timestamp,
    };
    use sha2::{Digest, Sha256};

    fn payload_hash(body: &[u8]) -> Sha256Hash {
        Sha256Hash::from_byte_array(Sha256::digest(body).into())
    }

    fn auth_event(method: &str, url: &str, payload_hash: Option<Sha256Hash>, keys: &Keys) -> Event {
        let http_method = HttpMethod::from_str(method).unwrap();
        let http_url = Url::from_str(url).unwrap();
        let mut http_data = HttpData::new(http_url, http_method);
        if let Some(hash) = payload_hash {
            http_data = http_data.payload(hash);
        }
        http_data
            .into_event_builder()
            .finalize(keys)
            .expect("Failed to sign event")
    }

    fn auth_header(event: &Event) -> String {
        format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(event).unwrap())
        )
    }

    fn request(method: &str, uri: &str, auth: Option<String>) -> Parts {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "localhost");
        if let Some(auth) = auth {
            builder = builder.header(AUTHORIZATION, auth);
        }
        builder.body(()).unwrap().into_parts().0
    }

    async fn extract(mut parts: Parts) -> Result<NostrAuth, AuthError> {
        NostrAuth::from_request_parts(&mut parts, &()).await
    }

    #[tokio::test]
    async fn valid_get_request_is_accepted() {
        let keys = Keys::generate();
        let event = auth_event("GET", "http://localhost/test", None, &keys);
        let auth = extract(request("GET", "/test", Some(auth_header(&event))))
            .await
            .unwrap();
        assert_eq!(auth.pubkey, keys.public_key());
        assert_eq!(auth.http_data.method, HttpMethod::GET);
    }

    #[tokio::test]
    async fn valid_post_with_payload_keeps_payload_hash() {
        let keys = Keys::generate();
        let payload_hash = payload_hash(br#"{"test": "data"}"#);
        let event = auth_event("POST", "http://localhost/test", Some(payload_hash), &keys);
        let auth = extract(request("POST", "/test", Some(auth_header(&event))))
            .await
            .unwrap();
        assert_eq!(auth.http_data.method, HttpMethod::POST);
        assert_eq!(auth.http_data.payload, Some(payload_hash));
    }

    #[tokio::test]
    async fn missing_and_malformed_headers_are_rejected() {
        assert!(matches!(
            extract(request("GET", "/test", None)).await,
            Err(AuthError::NoAuthHeader)
        ));
        assert!(matches!(
            extract(request("GET", "/test", Some("InvalidFormat".into()))).await,
            Err(AuthError::InvalidAuthFormat)
        ));
        assert!(matches!(
            extract(request(
                "GET",
                "/test",
                Some("Nostr invalid-base64!".into())
            ))
            .await,
            Err(AuthError::InvalidBase64(_))
        ));
        let invalid_json = format!("Nostr {}", BASE64.encode("not valid json"));
        assert!(matches!(
            extract(request("GET", "/test", Some(invalid_json))).await,
            Err(AuthError::InvalidEventJson(_))
        ));
    }

    #[tokio::test]
    async fn non_auth_event_kind_is_rejected() {
        let keys = Keys::generate();
        let tags = vec![
            Tag::parse(["method", "GET"]).unwrap(),
            Tag::parse(["u", "http://localhost/test"]).unwrap(),
        ];
        let event = EventBuilder::new(Kind::TextNote, "")
            .tags(tags)
            .finalize(&keys)
            .expect("Failed to sign event");
        assert!(matches!(
            extract(request("GET", "/test", Some(auth_header(&event)))).await,
            Err(AuthError::InvalidEventKind)
        ));
    }

    #[tokio::test]
    async fn expired_timestamp_is_rejected() {
        let keys = Keys::generate();
        let expired = (OffsetDateTime::now_utc() - time::Duration::hours(1)).unix_timestamp();
        let http_data = HttpData::new(
            Url::from_str("http://localhost/test").unwrap(),
            HttpMethod::GET,
        );
        let event = http_data
            .into_event_builder()
            .custom_created_at(Timestamp::from(expired as u64))
            .finalize(&keys)
            .unwrap();
        assert!(matches!(
            extract(request("GET", "/test", Some(auth_header(&event)))).await,
            Err(AuthError::ExpiredTimestamp)
        ));
    }

    #[tokio::test]
    async fn url_and_method_mismatches_are_rejected() {
        let keys = Keys::generate();
        let event = auth_event("GET", "http://localhost/different-path", None, &keys);
        assert!(matches!(
            extract(request("GET", "/test", Some(auth_header(&event)))).await,
            Err(AuthError::UrlMethodMismatch)
        ));
        let event = auth_event("POST", "http://localhost/test", None, &keys);
        assert!(matches!(
            extract(request("GET", "/test", Some(auth_header(&event)))).await,
            Err(AuthError::UrlMethodMismatch)
        ));
    }

    #[tokio::test]
    async fn tampered_event_fails_signature_verification() {
        let keys = Keys::generate();
        let event = auth_event("GET", "http://localhost/test", None, &keys);
        let mut value: serde_json::Value = serde_json::to_value(&event).unwrap();
        value["pubkey"] = serde_json::Value::String(Keys::generate().public_key().to_hex());
        let header = format!("Nostr {}", BASE64.encode(value.to_string()));
        assert!(matches!(
            extract(request("GET", "/test", Some(header))).await,
            Err(AuthError::InvalidSignature(_))
        ));
    }
}
