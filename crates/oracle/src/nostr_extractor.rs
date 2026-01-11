use axum::{
    extract::{FromRequestParts, OriginalUri},
    http::request::Parts,
    response::IntoResponse,
    Json,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use hyper::{header::AUTHORIZATION, StatusCode};
use log::{info, warn};
use nostr_sdk::{
    nips::nip98::{HttpData, HttpMethod},
    Event, Kind, PublicKey, Url,
};
use serde::{ser::SerializeStruct, Serialize, Serializer};
use serde_json::json;
use std::str::FromStr;
use time::OffsetDateTime;

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
        if (now - (event.created_at.as_u64() as i64)).abs() > 60 {
            return Err(AuthError::ExpiredTimestamp);
        }

        let tags = event.tags.clone().to_vec();
        let http_data =
            HttpData::try_from(tags).map_err(|e| AuthError::InvalidHttpData(e.to_string()))?;
        info!("Received request URI: {}", original_uri);
        let reconstructed_url = format!(
            "{}://{}{}",
            if parts.headers.contains_key("x-forwarded-proto") {
                "https"
            } else {
                "http"
            },
            parts
                .headers
                .get("host")
                .and_then(|h| h.to_str().ok())
                .unwrap_or(""),
            original_uri
        );
        info!(
            "reconstructed_url: {}, http_data.url: {}",
            reconstructed_url, http_data.url
        );
        info!(
            "http_data.method: {}, parts.method: {}",
            http_data.method,
            parts.method.as_str()
        );
        if http_data.url != Url::from_str(&reconstructed_url)?
            || http_data.method
                != HttpMethod::from_str(parts.method.as_str())
                    .map_err(|e| AuthError::InvalidMethod(e.to_string()))?
        {
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
    #[error("Invalid login")]
    InvalidLogin,
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

impl From<nostr_sdk::types::ParseError> for AuthError {
    fn from(err: nostr_sdk::types::ParseError) -> Self {
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
            Self::InvalidLogin => "invalid_login",
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
        let (body, code) = match &self {
            Self::InvalidSignature(_) => {
                warn!("{}", self);
                (json!({ "error": self }), StatusCode::FORBIDDEN)
            }
            Self::NoAuthHeader
            | Self::InvalidEventKind
            | Self::ExpiredTimestamp
            | Self::UrlMethodMismatch
            | Self::InvalidUrl(_)
            | Self::InvalidLogin
            | Self::InvalidMethod(_) => {
                warn!("{}", self);
                (json!({ "error": self }), StatusCode::UNAUTHORIZED)
            }
            _ => {
                warn!("{}", self);
                (json!({ "error": self }), StatusCode::BAD_REQUEST)
            }
        };

        (code, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use nostr_sdk::{
        hashes::{sha256::Hash as Sha256Hash, Hash},
        Alphabet, EventBuilder, Keys, SingleLetterTag, Tag, TagKind, Timestamp,
    };
    use std::{str::FromStr, sync::Arc};
    #[derive(Clone)]
    pub struct AppState;

    async fn create_auth_event(
        method: &str,
        url: &str,
        payload_hash: Option<Sha256Hash>,
        keys: &Keys,
    ) -> Event {
        let http_method = HttpMethod::from_str(method).unwrap();
        let http_url = Url::from_str(url).unwrap();
        let mut http_data = HttpData::new(http_url, http_method);

        if let Some(hash) = payload_hash {
            http_data = http_data.payload(hash);
        }

        EventBuilder::http_auth(http_data)
            .sign_with_keys(keys)
            .expect("Failed to sign event")
    }

    #[tokio::test]
    async fn test_valid_get_request() {
        let keys = Keys::generate();
        let state = AppState;

        let event = create_auth_event("GET", "http://localhost/test", None, &keys).await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(result.is_ok());
        let auth = result.unwrap();
        assert_eq!(auth.pubkey, keys.public_key());
        assert_eq!(auth.http_data.method, HttpMethod::GET);
    }

    #[tokio::test]
    async fn test_valid_post_with_payload() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let body = r#"{"test": "data"}"#;
        let payload_hash = Sha256Hash::hash(body.as_bytes());

        let event =
            create_auth_event("POST", "http://localhost/test", Some(payload_hash), &keys).await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .method("POST")
            .uri("/test")
            .header("host", "localhost")
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(result.is_ok());
        let auth = result.unwrap();
        assert_eq!(auth.pubkey, keys.public_key());
        assert_eq!(auth.http_data.method, HttpMethod::POST);
        assert_eq!(auth.http_data.payload, Some(payload_hash));
    }

    #[tokio::test]
    async fn test_missing_auth_header() {
        let state = Arc::new(AppState);

        let req = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::NoAuthHeader)));
    }

    #[tokio::test]
    async fn test_invalid_auth_format() {
        let state = Arc::new(AppState);

        let req = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .header(AUTHORIZATION, "InvalidFormat")
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::InvalidAuthFormat)));
    }

    #[tokio::test]
    async fn test_invalid_base64() {
        let state = Arc::new(AppState);

        let req = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .header(AUTHORIZATION, "Nostr invalid-base64!")
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::InvalidBase64(_))));
    }

    #[tokio::test]
    async fn test_invalid_event_json() {
        let state = Arc::new(AppState);

        let invalid_json = BASE64.encode("not valid json");
        let req = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .header(AUTHORIZATION, format!("Nostr {invalid_json}"))
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::InvalidEventJson(_))));
    }

    #[tokio::test]
    async fn test_non_auth_event_kind() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);
        let http_method = HttpMethod::from_str("GET").unwrap();
        let http_url = Url::from_str("http://localhost/test").unwrap();

        let tags = vec![
            Tag::custom(TagKind::Method, [http_method.to_string()]),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::U)),
                [http_url.to_string()],
            ),
        ];

        let created_at = OffsetDateTime::now_utc().unix_timestamp() as u64;

        // Create a regular text note instead of auth event
        let event = EventBuilder::new(Kind::TextNote, "")
            .custom_created_at(Timestamp::from(created_at))
            .tags(tags)
            .sign_with_keys(&keys)
            .expect("Failed to sign event");

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::InvalidEventKind)));
    }

    #[tokio::test]
    async fn test_expired_timestamp() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let expired_time =
            (OffsetDateTime::now_utc() - time::Duration::hours(1)).unix_timestamp() as u64;
        let http_data = HttpData::new(
            Url::from_str("http://localhost/test").unwrap(),
            HttpMethod::GET,
        );

        let event = EventBuilder::http_auth(http_data)
            .custom_created_at(Timestamp::from(expired_time))
            .sign_with_keys(&keys)
            .unwrap();

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::ExpiredTimestamp)));
    }

    #[tokio::test]
    async fn test_url_mismatch() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let event = create_auth_event("GET", "http://localhost/different-path", None, &keys).await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::UrlMethodMismatch)));
    }

    #[tokio::test]
    async fn test_method_mismatch() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let event = create_auth_event("POST", "http://localhost/test", None, &keys).await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .method("GET") // Different method from event
            .uri("/test")
            .header("host", "localhost")
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::UrlMethodMismatch)));
    }

    #[tokio::test]
    async fn test_non_empty_content() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let http_method = HttpMethod::from_str("GET").unwrap();
        let http_url = Url::from_str("http://localhost/test").unwrap();

        let tags = vec![
            Tag::custom(TagKind::Method, [http_method.to_string()]),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::U)),
                [http_url.to_string()],
            ),
        ];

        let created_at = OffsetDateTime::now_utc().unix_timestamp() as u64;

        // Create event with non-empty content (invalid per NIP-98)
        let event = EventBuilder::new(Kind::HttpAuth, "non-empty content")
            .custom_created_at(Timestamp::from(created_at))
            .tags(tags)
            .sign_with_keys(&keys)
            .expect("Failed to sign event");

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(matches!(result, Err(AuthError::NonEmptyContent)));
    }

    #[tokio::test]
    async fn test_forwarded_proto() {
        let keys = Keys::generate();
        let state = Arc::new(AppState);

        let event = create_auth_event(
            "GET",
            "https://localhost/test", // Note https
            None,
            &keys,
        )
        .await;

        let auth_header = format!(
            "Nostr {}",
            BASE64.encode(serde_json::to_string(&event).unwrap())
        );

        let req = Request::builder()
            .method("GET")
            .uri("/test")
            .header("host", "localhost")
            .header("x-forwarded-proto", "https")
            .header(AUTHORIZATION, auth_header)
            .body(())
            .unwrap();

        let result = NostrAuth::from_request_parts(&mut req.into_parts().0, &state).await;

        assert!(result.is_ok());
    }
}
