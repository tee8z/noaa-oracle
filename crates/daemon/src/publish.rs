//! Publishing data files: upload to the oracle and archive to S3.
//!
//! Each file is `{dataset}_{rfc3339 UTC}.parquet` under
//! `{data_dir}/{YYYY-MM-DD}/`. A successful upload writes a `.uploaded`
//! marker beside it and a successful archive an `.archived` marker, so a
//! file whose publication failed is retried on the next run and a file is
//! deleted only after it is fully published and older than the retention
//! period.
//!
//! Uploads are the raw parquet bytes, signed with NIP-98 over the exact
//! request URL, method, and body hash by the daemon's key.

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use nostr::{
    event::{FinalizeEvent, IntoEventBuilder},
    key::Keys,
    nips::nip98::{HttpData, HttpMethod, Sha256Hash},
};
use reqwest::{Client, StatusCode, Url, header};
use sha2::{Digest, Sha256};
use slog::{Logger, info, warn};
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use time::{Date, OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{S3Storage, keys::npub};

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const UPLOADED: &str = "uploaded";
const ARCHIVED: &str = "archived";

/// A data file ready to publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub path: PathBuf,
    /// `{dataset}_{rfc3339}.parquet`; the oracle's file name.
    pub name: String,
    pub generated_at: OffsetDateTime,
}

impl Artifact {
    /// Reads the name back from a path, if it is one of ours.
    pub fn from_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?.to_owned();
        let stem = name.strip_suffix(".parquet")?;
        let (_, timestamp) = stem.split_once('_')?;
        let generated_at = OffsetDateTime::parse(timestamp, &Rfc3339).ok()?;
        Some(Self {
            path: path.to_owned(),
            name,
            generated_at,
        })
    }

    fn marker(&self, kind: &str) -> PathBuf {
        let mut marker = self.path.clone().into_os_string();
        marker.push(format!(".{kind}"));
        PathBuf::from(marker)
    }

    fn has(&self, kind: &str) -> bool {
        self.marker(kind).exists()
    }

    async fn mark(&self, kind: &str) -> Result<(), PublishError> {
        tokio::fs::write(self.marker(kind), b"")
            .await
            .map_err(PublishError::Marker)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("reading {path} failed")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("upload request failed")]
    Request(#[source] reqwest::Error),
    #[error(
        "the oracle refused the daemon's signature ({status}); add {npub} to the oracle's uploader_pubkeys and make base_url equal its remote_url"
    )]
    Unauthorized { status: StatusCode, npub: String },
    #[error("the oracle rejected {name}: {status} {body}")]
    Rejected {
        name: String,
        status: StatusCode,
        body: String,
    },
    #[error("the oracle is unavailable ({status}); will retry")]
    Unavailable { status: StatusCode },
    #[error("archiving to S3 failed: {0}")]
    Archive(String),
    #[error("writing a publication marker failed")]
    Marker(#[source] io::Error),
    #[error("signing the upload failed: {0}")]
    Sign(String),
}

pub struct Publisher {
    client: Client,
    base_url: Url,
    keys: Keys,
    s3: Option<S3Storage>,
    logger: Logger,
}

impl Publisher {
    pub fn new(
        base_url: Url,
        keys: Keys,
        s3: Option<S3Storage>,
        logger: Logger,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: Client::builder()
                .timeout(UPLOAD_TIMEOUT)
                .connect_timeout(Duration::from_secs(15))
                .build()?,
            base_url,
            keys,
            s3,
            logger,
        })
    }

    /// Uploads and archives `artifact`, skipping steps already done.
    pub async fn publish(&self, artifact: &Artifact) -> Result<(), PublishError> {
        if !artifact.has(UPLOADED) {
            self.upload(artifact).await?;
            artifact.mark(UPLOADED).await?;
            info!(self.logger, "uploaded {} to the oracle", artifact.name);
        }
        if let Some(s3) = &self.s3
            && !artifact.has(ARCHIVED)
        {
            s3.upload_parquet(
                &artifact.path,
                &artifact.generated_at.date().to_string(),
                &artifact.name,
            )
            .await
            .map_err(|error| PublishError::Archive(format!("{error:#}")))?;
            artifact.mark(ARCHIVED).await?;
        }
        Ok(())
    }

    fn is_published(&self, artifact: &Artifact) -> bool {
        artifact.has(UPLOADED) && (self.s3.is_none() || artifact.has(ARCHIVED))
    }

    async fn upload(&self, artifact: &Artifact) -> Result<(), PublishError> {
        let body = tokio::fs::read(&artifact.path)
            .await
            .map_err(|source| PublishError::Read {
                path: artifact.path.clone(),
                source,
            })?;
        let url = self
            .base_url
            .join(&format!("file/{}", artifact.name))
            .map_err(|_| PublishError::Rejected {
                name: artifact.name.clone(),
                status: StatusCode::BAD_REQUEST,
                body: String::from("file name does not form a URL"),
            })?;
        let authorization = self.authorization(&url, &body)?;
        let response = self
            .client
            .post(url)
            .header(header::CONTENT_TYPE, "application/vnd.apache.parquet")
            .header(header::AUTHORIZATION, authorization)
            .body(body)
            .send()
            .await
            .map_err(PublishError::Request)?;
        let status = response.status();
        match status {
            status if status.is_success() => Ok(()),
            // Already published by an earlier attempt whose reply was lost.
            StatusCode::CONFLICT => Ok(()),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(PublishError::Unauthorized {
                status,
                npub: npub(&self.keys),
            }),
            status if status.is_server_error() => Err(PublishError::Unavailable { status }),
            status => Err(PublishError::Rejected {
                name: artifact.name.clone(),
                status,
                body: response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(512)
                    .collect(),
            }),
        }
    }

    /// `Nostr <base64 event>` over the URL, `POST`, and the body hash. Each
    /// call signs anew. The event id covers `created_at` in whole seconds,
    /// so the oracle treats a repeat of the same upload within one second
    /// as a replay; retries happen on the next run.
    fn authorization(&self, url: &Url, body: &[u8]) -> Result<String, PublishError> {
        let payload = Sha256Hash::from_byte_array(Sha256::digest(body).into());
        let event = HttpData::new(url.clone(), HttpMethod::POST)
            .payload(payload)
            .into_event_builder()
            .finalize(&self.keys)
            .map_err(|error| PublishError::Sign(error.to_string()))?;
        let json =
            serde_json::to_string(&event).map_err(|error| PublishError::Sign(error.to_string()))?;
        Ok(format!("Nostr {}", BASE64.encode(json)))
    }

    /// Retries files an earlier run could not publish. Returns how many are
    /// still unpublished.
    pub async fn publish_pending(&self, data_dir: &Path) -> usize {
        let mut failed = 0;
        for artifact in artifacts(data_dir) {
            if self.is_published(&artifact) {
                continue;
            }
            if let Err(error) = self.publish(&artifact).await {
                failed += 1;
                warn!(
                    self.logger,
                    "republishing {} failed: {:#}",
                    artifact.name,
                    anyhow::Error::from(error)
                );
            }
        }
        failed
    }

    /// Deletes fully published files older than `retention`, with their
    /// markers, and removes emptied date directories.
    pub fn prune(&self, data_dir: &Path, retention: Duration, now: OffsetDateTime) -> usize {
        let cutoff = now - retention;
        let mut removed = 0;
        for artifact in artifacts(data_dir) {
            if artifact.generated_at >= cutoff || !self.is_published(&artifact) {
                continue;
            }
            for path in [
                artifact.path.clone(),
                artifact.marker(UPLOADED),
                artifact.marker(ARCHIVED),
            ] {
                let _ = std::fs::remove_file(path);
            }
            removed += 1;
            if let Some(parent) = artifact.path.parent() {
                // Only succeeds when the directory is empty.
                let _ = std::fs::remove_dir(parent);
            }
        }
        removed
    }
}

/// Every data file under `{data_dir}/{YYYY-MM-DD}/`.
fn artifacts(data_dir: &Path) -> Vec<Artifact> {
    let date = time::macros::format_description!("[year]-[month]-[day]");
    let Ok(days) = std::fs::read_dir(data_dir) else {
        return vec![];
    };
    let mut found: Vec<Artifact> = days
        .filter_map(Result::ok)
        .filter(|day| {
            day.file_name()
                .to_str()
                .is_some_and(|name| Date::parse(name, &date).is_ok())
        })
        .filter_map(|day| std::fs::read_dir(day.path()).ok())
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|file| Artifact::from_path(&file.path()))
        .collect();
    found.sort_by_key(|artifact| artifact.generated_at);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{event::Event, nips::nip98::HttpData};

    fn logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn publisher(keys: Keys) -> Publisher {
        Publisher::new(
            Url::parse("https://oracle.test").unwrap(),
            keys,
            None,
            logger(),
        )
        .unwrap()
    }

    #[test]
    fn uploads_are_signed_over_the_url_method_and_body() {
        let keys = Keys::generate();
        let publisher = publisher(keys.clone());
        let url = Url::parse("https://oracle.test/file/observations_2030-01-01T00:00:00Z.parquet")
            .unwrap();
        let body = b"PAR1 rows PAR1";
        let header = publisher.authorization(&url, body).unwrap();
        let json = BASE64
            .decode(header.strip_prefix("Nostr ").unwrap())
            .unwrap();
        let event: Event = serde_json::from_slice(&json).unwrap();
        event.verify().unwrap();
        assert_eq!(event.pubkey, keys.public_key());
        let signed = HttpData::try_from(event.tags.to_vec()).unwrap();
        assert_eq!(signed.url, url);
        assert_eq!(signed.method, HttpMethod::POST);
        assert_eq!(
            signed.payload,
            Some(Sha256Hash::from_byte_array(Sha256::digest(body).into()))
        );
        assert_ne!(
            publisher.authorization(&url, body).unwrap(),
            header,
            "each call produces a new signature"
        );
    }

    #[test]
    fn only_published_files_older_than_retention_are_pruned() {
        let directory = tempfile::tempdir().unwrap();
        let day = directory.path().join("2030-01-01");
        std::fs::create_dir_all(&day).unwrap();
        let old = day.join("observations_2030-01-01T00:00:00Z.parquet");
        let unpublished = day.join("forecasts_2030-01-01T00:00:00Z.parquet");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&unpublished, b"x").unwrap();
        std::fs::write(
            day.join("observations_2030-01-01T00:00:00Z.parquet.uploaded"),
            b"",
        )
        .unwrap();
        std::fs::write(day.join("notes.txt"), b"").unwrap();
        let publisher = publisher(Keys::generate());
        let now = time::macros::datetime!(2030-01-10 00:00 UTC);
        assert_eq!(
            publisher.prune(directory.path(), Duration::from_secs(86400), now),
            1
        );
        assert!(!old.exists());
        assert!(unpublished.exists(), "unpublished files are kept for retry");
        let recent = time::macros::datetime!(2030-01-01 12:00 UTC);
        std::fs::write(
            day.join("forecasts_2030-01-01T00:00:00Z.parquet.uploaded"),
            b"",
        )
        .unwrap();
        assert_eq!(
            publisher.prune(directory.path(), Duration::from_secs(7 * 86400), recent),
            0,
            "files inside the retention period stay"
        );
    }

    #[test]
    fn artifacts_are_read_back_from_their_names() {
        let artifact = Artifact::from_path(Path::new(
            "/d/2030-01-01/forecasts_2030-01-01T05:00:00.5Z.parquet",
        ))
        .unwrap();
        assert_eq!(artifact.name, "forecasts_2030-01-01T05:00:00.5Z.parquet");
        assert!(Artifact::from_path(Path::new("/d/x.parquet.uploaded")).is_none());
        assert!(Artifact::from_path(Path::new("/d/forecasts_nope.parquet")).is_none());
    }
}
