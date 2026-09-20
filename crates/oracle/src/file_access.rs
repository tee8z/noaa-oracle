//! Parquet file discovery and streaming from local disk or S3.
//!
//! File names follow `<observations|forecasts>_<rfc3339>.parquet` and are
//! stored under a `YYYY-MM-DD` directory of their generation date. Names that
//! do not match are ignored on listing and rejected on upload, because the
//! oracle interpolates them into DuckDB `read_parquet` calls.

use async_trait::async_trait;
use axum::body::Body;
use log::trace;
use serde::{Deserialize, Serialize};
use time::{
    Date, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339,
    macros::format_description,
};
use tokio::fs;
use tokio_util::io::ReaderStream;
use utoipa::IntoParams;

#[derive(Clone, Deserialize, Serialize, IntoParams)]
pub struct FileParams {
    #[serde(with = "time::serde::rfc3339::option")]
    pub start: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub end: Option<OffsetDateTime>,
    pub observations: Option<bool>,
    pub forecasts: Option<bool>,
}

pub struct FileAccess {
    data_dir: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Observations,
    Forecasts,
}

impl FileKind {
    fn prefix(self) -> &'static str {
        match self {
            FileKind::Observations => "observations",
            FileKind::Forecasts => "forecasts",
        }
    }
}

/// A validated parquet file name: `<kind>_<rfc3339>.parquet`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParquetFileName {
    pub kind: FileKind,
    pub generated_at: OffsetDateTime,
}

impl ParquetFileName {
    /// Accepts only the daemon's naming scheme. Anything else, including
    /// path separators, quotes, or unknown prefixes, is rejected.
    pub fn parse(name: &str) -> Result<Self, Error> {
        let invalid = || Error::InvalidFileName(name.to_string());
        let stem = name.strip_suffix(".parquet").ok_or_else(invalid)?;
        let (prefix, timestamp) = stem.split_once('_').ok_or_else(invalid)?;
        let kind = match prefix {
            "observations" => FileKind::Observations,
            "forecasts" => FileKind::Forecasts,
            _ => return Err(invalid()),
        };
        if !timestamp
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':' | b'.' | b'+'))
        {
            return Err(invalid());
        }
        let generated_at = OffsetDateTime::parse(timestamp, &Rfc3339).map_err(|_| invalid())?;
        Ok(Self { kind, generated_at })
    }

    fn matches(&self, params: &FileParams) -> bool {
        let wanted = match self.kind {
            FileKind::Observations => params.observations,
            FileKind::Forecasts => params.forecasts,
        };
        let any_kind = params.forecasts.is_none() && params.observations.is_none();
        (any_kind || wanted == Some(true)) && is_time_in_range(self.generated_at, params)
    }
}

impl std::fmt::Display for ParquetFileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let timestamp = self
            .generated_at
            .format(&Rfc3339)
            .map_err(|_| std::fmt::Error)?;
        write!(f, "{}_{}.parquet", self.kind.prefix(), timestamp)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Failed to format time string: {0}")]
    TimeFormat(#[from] time::error::Format),
    #[error("Failed to parse time string: {0}")]
    TimeParse(#[from] time::error::Parse),
    #[error("File not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(String),
    #[error("Invalid parquet file name: {0:?}")]
    InvalidFileName(String),
    #[error("listing files needs a start and end date at most {MAX_LIST_DAYS} days apart")]
    UnboundedListing,
}

/// Requested UTC days one S3 listing may cover, plus two offset boundary prefixes.
pub const MAX_LIST_DAYS: usize = 366;

/// Where parquet files live. Implementations return only validated file
/// names, so callers can pass them straight to [`FileData::build_file_paths`].
#[async_trait]
pub trait FileData: Send + Sync {
    async fn grab_file_names(&self, params: FileParams) -> Result<Vec<String>, Error>;
    fn build_file_paths(&self, file_names: Vec<String>) -> Vec<String>;
    fn build_file_path(&self, file: &ParquetFileName) -> String;
    /// Download a file and return its contents as an axum Body stream
    async fn download_file(&self, file: &ParquetFileName) -> Result<Body, Error>;
}

impl FileAccess {
    pub fn new(data_dir: String) -> Self {
        Self { data_dir }
    }
}

#[async_trait]
impl FileData for FileAccess {
    fn build_file_paths(&self, file_names: Vec<String>) -> Vec<String> {
        file_names
            .iter()
            .filter_map(|file_name| ParquetFileName::parse(file_name).ok())
            .map(|file| self.build_file_path(&file))
            .collect()
    }

    fn build_file_path(&self, file: &ParquetFileName) -> String {
        format!("{}/{}/{}", self.data_dir, file.generated_at.date(), file)
    }

    async fn download_file(&self, file: &ParquetFileName) -> Result<Body, Error> {
        let file_path = self.build_file_path(file);
        let file = tokio::fs::File::open(&file_path)
            .await
            .map_err(|e| Error::NotFound(format!("{}: {}", file_path, e)))?;
        let stream = ReaderStream::new(file);
        Ok(Body::from_stream(stream))
    }

    async fn grab_file_names(&self, params: FileParams) -> Result<Vec<String>, Error> {
        let mut file_names = vec![];
        let Ok(mut entries) = fs::read_dir(&self.data_dir).await else {
            return Ok(file_names);
        };
        let date_format = format_description!("[year]-[month]-[day]");
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(date) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(directory_date) = Date::parse(&date, &date_format) else {
                trace!("skipping non-date directory {date}");
                continue;
            };
            if !is_date_in_range(directory_date, &params) {
                continue;
            }
            let Ok(mut files) = fs::read_dir(path).await else {
                continue;
            };
            while let Ok(Some(file)) = files.next_entry().await {
                let name = file.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                match ParquetFileName::parse(name) {
                    Ok(parsed) if parsed.matches(&params) => file_names.push(name.to_owned()),
                    Ok(_) => {}
                    Err(_) => trace!("skipping unrecognised file {name}"),
                }
            }
        }
        Ok(file_names)
    }
}

fn is_date_in_range(compare_to: Date, params: &FileParams) -> bool {
    // Existing keys use the timestamp's own calendar date. An offset timestamp
    // can live in either adjacent UTC date directory; matches() filters instants.
    let after_start = params
        .start
        .map(|start| compare_to >= first_directory_date(start))
        .unwrap_or(true);
    let before_end = params
        .end
        .map(|end| compare_to <= last_directory_date(end))
        .unwrap_or(true);
    after_start && before_end
}

fn first_directory_date(time: OffsetDateTime) -> Date {
    let day = time.to_offset(UtcOffset::UTC).date();
    day.previous_day().unwrap_or(day)
}

fn last_directory_date(time: OffsetDateTime) -> Date {
    let day = time.to_offset(UtcOffset::UTC).date();
    day.next_day().unwrap_or(day)
}

fn listing_prefixes(params: &FileParams) -> Result<Vec<String>, Error> {
    let (Some(start), Some(end)) = (params.start, params.end) else {
        return Err(Error::UnboundedListing);
    };
    let requested_days = (end.to_offset(UtcOffset::UTC).date()
        - start.to_offset(UtcOffset::UTC).date())
    .whole_days()
        + 1;
    if start > end || requested_days > MAX_LIST_DAYS as i64 {
        return Err(Error::UnboundedListing);
    }
    Ok(
        std::iter::successors(Some(first_directory_date(start)), |day| day.next_day())
            .take_while(|day| *day <= last_directory_date(end))
            .map(|day| format!("weather_data/{day}/"))
            .collect(),
    )
}

fn is_time_in_range(compare_to: OffsetDateTime, params: &FileParams) -> bool {
    let after_start = params
        .start
        .map(|start| compare_to >= start)
        .unwrap_or(true);
    let before_end = params.end.map(|end| compare_to <= end).unwrap_or(true);
    after_start && before_end
}

/// S3-backed file access for listing and downloading parquet files.
/// S3 key format: weather_data/{YYYY-MM-DD}/{filename}
pub struct S3FileAccess {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3FileAccess {
    pub async fn new(bucket: String, endpoint: Option<String>) -> Self {
        let mut config_loader = aws_config::from_env();
        if let Some(endpoint_url) = endpoint {
            log::info!("Using custom S3 endpoint: {}", endpoint_url);
            config_loader = config_loader.endpoint_url(endpoint_url);
        }
        let config = config_loader.load().await;
        let client = aws_sdk_s3::Client::new(&config);
        log::info!("S3 file access initialized for bucket: {}", bucket);
        Self { client, bucket }
    }

    /// Build the S3 key for a file
    fn s3_key(file: &ParquetFileName) -> String {
        format!("weather_data/{}/{}", file.generated_at.date(), file)
    }
}

#[async_trait]
impl FileData for S3FileAccess {
    async fn grab_file_names(&self, params: FileParams) -> Result<Vec<String>, Error> {
        let mut file_names = Vec::new();

        // One prefix listing per day; the whole bucket is never listed.
        let prefixes = listing_prefixes(&params)?;

        for prefix in &prefixes {
            let mut continuation_token: Option<String> = None;
            loop {
                let mut req = self
                    .client
                    .list_objects_v2()
                    .bucket(&self.bucket)
                    .prefix(prefix);

                if let Some(token) = &continuation_token {
                    req = req.continuation_token(token);
                }

                let resp = req.send().await.map_err(|e| {
                    Error::Io(format!(
                        "S3 list_objects_v2 failed for prefix '{prefix}': {e}"
                    ))
                })?;

                for obj in resp.contents() {
                    // Key layout: weather_data/2026-02-16/forecasts_2026-02-16T10:00:00Z.parquet
                    let Some(filename) = obj.key().and_then(|key| key.rsplit('/').next()) else {
                        continue;
                    };
                    match ParquetFileName::parse(filename) {
                        Ok(parsed) if parsed.matches(&params) => {
                            file_names.push(filename.to_string());
                        }
                        Ok(_) => {}
                        Err(_) => trace!("skipping unrecognised S3 object {filename}"),
                    }
                }

                if resp.is_truncated() == Some(true) {
                    continuation_token = resp.next_continuation_token().map(|s| s.to_string());
                } else {
                    break;
                }
            }
        }

        Ok(file_names)
    }

    fn build_file_paths(&self, file_names: Vec<String>) -> Vec<String> {
        file_names
            .iter()
            .filter_map(|file_name| ParquetFileName::parse(file_name).ok())
            .map(|file| Self::s3_key(&file))
            .collect()
    }

    fn build_file_path(&self, file: &ParquetFileName) -> String {
        Self::s3_key(file)
    }

    async fn download_file(&self, file: &ParquetFileName) -> Result<Body, Error> {
        let key = Self::s3_key(file);
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|error| {
                let service = error.into_service_error();
                if service.is_no_such_key() {
                    Error::NotFound(key.clone())
                } else {
                    Error::Io(format!("S3 get_object '{key}': {service}"))
                }
            })?;
        // Stream the object instead of buffering it in memory.
        Ok(Body::from_stream(ReaderStream::new(
            resp.body.into_async_read(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(start: &str, end: &str) -> FileParams {
        FileParams {
            start: Some(OffsetDateTime::parse(start, &Rfc3339).unwrap()),
            end: Some(OffsetDateTime::parse(end, &Rfc3339).unwrap()),
            observations: Some(true),
            forecasts: Some(false),
        }
    }

    #[test]
    fn s3_prefixes_cover_offset_dates_without_unbounded_listing() {
        let params = bounds("2026-01-22T00:00:00+02:00", "2026-01-22T00:30:00+02:00");
        assert_eq!(
            listing_prefixes(&params).unwrap(),
            vec![
                "weather_data/2026-01-20/",
                "weather_data/2026-01-21/",
                "weather_data/2026-01-22/",
            ]
        );
        let mut full_year = bounds("2024-01-01T00:00:00Z", "2024-12-31T23:59:59Z");
        assert_eq!(
            listing_prefixes(&full_year).unwrap().len(),
            MAX_LIST_DAYS + 2
        );
        full_year.end = full_year.end.map(|end| end + time::Duration::days(1));
        assert!(listing_prefixes(&full_year).is_err());
        assert!(listing_prefixes(&bounds("2026-01-22T00:00:00Z", "2026-01-21T00:00:00Z")).is_err());
        full_year.start = None;
        assert!(listing_prefixes(&full_year).is_err());
    }

    #[tokio::test]
    async fn lists_offset_files_by_instant_across_directory_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let access = FileAccess::new(directory.path().to_string_lossy().into_owned());
        let names = [
            "observations_2026-01-21T22:00:00Z.parquet",
            "observations_2026-01-22T00:15:00+02:00.parquet",
            "observations_2026-01-21T21:59:59Z.parquet",
            "observations_2026-01-22T00:31:00+02:00.parquet",
        ];
        for name in names {
            let file = ParquetFileName::parse(name).unwrap();
            std::fs::create_dir_all(directory.path().join(file.generated_at.date().to_string()))
                .unwrap();
            std::fs::write(access.build_file_path(&file), b"").unwrap();
        }
        for params in [
            bounds("2026-01-22T00:00:00+02:00", "2026-01-22T00:30:00+02:00"),
            bounds("2026-01-21T22:00:00Z", "2026-01-21T22:30:00Z"),
        ] {
            let mut files = access.grab_file_names(params).await.unwrap();
            files.sort();
            assert_eq!(files, names[..2]);
        }
    }

    #[test]
    fn parses_daemon_file_names_and_rejects_everything_else() {
        let name = "observations_2026-01-21T23:59:43.269662415Z.parquet";
        let parsed = ParquetFileName::parse(name).unwrap();
        assert_eq!(parsed.kind, FileKind::Observations);
        assert_eq!(parsed.generated_at.date().to_string(), "2026-01-21");
        assert_eq!(parsed.to_string(), name);
        assert_eq!(
            ParquetFileName::parse("forecasts_2026-01-21T15:59:43.858149618Z.parquet")
                .unwrap()
                .kind,
            FileKind::Forecasts
        );
        for invalid in [
            "invalid.parquet",
            "observations_notadate.parquet",
            "../observations_2026-01-21T23:59:43Z.parquet",
            "observations_2026-01-21T23:59:43Z.parquet']) UNION SELECT 1 --",
            "metrics_2026-01-21T23:59:43Z.parquet",
            "observations_2026-01-21T23:59:43Z.csv",
        ] {
            assert!(ParquetFileName::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn lists_only_matching_files_from_dated_directories() {
        let directory = tempfile::tempdir().unwrap();
        let day = directory.path().join("2026-01-21");
        std::fs::create_dir_all(&day).unwrap();
        for name in [
            "observations_2026-01-21T10:00:00Z.parquet",
            "forecasts_2026-01-21T10:00:00Z.parquet",
            "notes.txt",
            "forecasts_2026-01-21T10:00:00Z.parquet.tmp",
        ] {
            std::fs::write(day.join(name), b"").unwrap();
        }
        std::fs::create_dir_all(directory.path().join("scratch")).unwrap();
        let access = FileAccess::new(directory.path().to_string_lossy().into_owned());
        let mut all = access
            .grab_file_names(FileParams {
                start: None,
                end: None,
                observations: None,
                forecasts: None,
            })
            .await
            .unwrap();
        all.sort();
        assert_eq!(
            all,
            vec![
                "forecasts_2026-01-21T10:00:00Z.parquet",
                "observations_2026-01-21T10:00:00Z.parquet"
            ]
        );
        let forecasts = access
            .grab_file_names(FileParams {
                start: Some(OffsetDateTime::parse("2026-01-21T00:00:00Z", &Rfc3339).unwrap()),
                end: Some(OffsetDateTime::parse("2026-01-22T00:00:00Z", &Rfc3339).unwrap()),
                observations: Some(false),
                forecasts: Some(true),
            })
            .await
            .unwrap();
        assert_eq!(forecasts, vec!["forecasts_2026-01-21T10:00:00Z.parquet"]);
        let paths = access.build_file_paths(forecasts);
        assert_eq!(
            paths,
            vec![format!(
                "{}/2026-01-21/forecasts_2026-01-21T10:00:00Z.parquet",
                directory.path().display()
            )]
        );
    }
}
