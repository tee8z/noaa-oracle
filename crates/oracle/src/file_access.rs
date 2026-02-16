use async_trait::async_trait;
use axum::body::Body;
use log::trace;
use serde::{Deserialize, Serialize};
use time::{
    format_description::well_known::Rfc3339, macros::format_description, Date, OffsetDateTime,
};
use tokio::fs;
use tokio_util::io::ReaderStream;
use utoipa::IntoParams;

use crate::{create_folder, subfolder_exists};

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
}

#[async_trait]
pub trait FileData: Send + Sync {
    async fn grab_file_names(&self, params: FileParams) -> Result<Vec<String>, Error>;
    fn current_folder(&self) -> String;
    fn build_file_paths(&self, file_names: Vec<String>) -> Vec<String>;
    fn build_file_path(&self, filename: &str, file_generated_at: OffsetDateTime) -> String;
    /// Download a file and return its contents as an axum Body stream
    async fn download_file(
        &self,
        filename: &str,
        file_generated_at: OffsetDateTime,
    ) -> Result<Body, Error>;
}

impl FileAccess {
    pub fn new(data_dir: String) -> Self {
        Self { data_dir }
    }

    fn add_filename(
        &self,
        entry: tokio::fs::DirEntry,
        params: &FileParams,
    ) -> Result<Option<String>, Error> {
        if let Some(filename) = entry.file_name().to_str() {
            let file_pieces: Vec<String> = filename.split('_').map(|f| f.to_owned()).collect();
            let created_time = drop_suffix(file_pieces.last().unwrap(), ".parquet");
            trace!("parsed file time:{}", created_time);

            let file_generated_at = OffsetDateTime::parse(&created_time, &Rfc3339)?;
            let valid_time_range = is_time_in_range(file_generated_at, params);
            let file_data_type = file_pieces.first().unwrap();
            trace!("parsed file type:{}", file_data_type);

            if let Some(observations) = params.observations {
                if observations && file_data_type.eq("observations") && valid_time_range {
                    return Ok(Some(filename.to_owned()));
                }
            }

            if let Some(forecasts) = params.forecasts {
                if forecasts && file_data_type.eq("forecasts") && valid_time_range {
                    return Ok(Some(filename.to_owned()));
                }
            }

            if params.forecasts.is_none() && params.observations.is_none() && valid_time_range {
                return Ok(Some(filename.to_owned()));
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl FileData for FileAccess {
    fn build_file_paths(&self, file_names: Vec<String>) -> Vec<String> {
        file_names
            .iter()
            .map(|file_name| {
                let file_pieces: Vec<String> = file_name.split('_').map(|f| f.to_owned()).collect();
                let created_time = drop_suffix(file_pieces.last().unwrap(), ".parquet");
                let file_generated_at = OffsetDateTime::parse(&created_time, &Rfc3339).unwrap();
                format!(
                    "{}/{}/{}",
                    self.data_dir,
                    file_generated_at.date(),
                    file_name
                )
            })
            .collect()
    }

    fn current_folder(&self) -> String {
        let current_date = OffsetDateTime::now_utc().date();
        let subfolder = format!("{}/{}", self.data_dir, current_date);
        if !subfolder_exists(&subfolder) {
            create_folder(&subfolder)
        }
        subfolder
    }

    fn build_file_path(&self, filename: &str, file_generated_at: OffsetDateTime) -> String {
        format!(
            "{}/{}/{}",
            self.data_dir,
            file_generated_at.date(),
            filename
        )
    }

    async fn download_file(
        &self,
        filename: &str,
        file_generated_at: OffsetDateTime,
    ) -> Result<Body, Error> {
        let file_path = self.build_file_path(filename, file_generated_at);
        let file = tokio::fs::File::open(&file_path)
            .await
            .map_err(|e| Error::NotFound(format!("{}: {}", file_path, e)))?;
        let stream = ReaderStream::new(file);
        Ok(Body::from_stream(stream))
    }

    async fn grab_file_names(&self, params: FileParams) -> Result<Vec<String>, Error> {
        let mut files_names = vec![];
        if let Ok(mut entries) = fs::read_dir(self.data_dir.clone()).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if let Some(date) = entry.file_name().to_str() {
                    let format = format_description!("[year]-[month]-[day]");
                    let directory_date = Date::parse(date, &format)?;
                    if !is_date_in_range(directory_date, &params) {
                        continue;
                    }

                    if let Ok(mut subentries) = fs::read_dir(path).await {
                        while let Ok(Some(subentries)) = subentries.next_entry().await {
                            if let Some(filename) = self.add_filename(subentries, &params)? {
                                files_names.push(filename);
                            }
                        }
                    }
                }
            }
        }
        Ok(files_names)
    }
}

pub fn drop_suffix(input: &str, suffix: &str) -> String {
    if let Some(stripped) = input.strip_suffix(suffix) {
        stripped.to_string()
    } else {
        input.to_string()
    }
}

fn is_date_in_range(compare_to: Date, params: &FileParams) -> bool {
    let after_start = params
        .start
        .map(|start| compare_to >= start.date())
        .unwrap_or(true);
    let before_end = params
        .end
        .map(|end| compare_to <= end.date())
        .unwrap_or(true);
    after_start && before_end
}

fn is_time_in_range(compare_to: OffsetDateTime, params: &FileParams) -> bool {
    let after_start = params
        .start
        .map(|start| compare_to >= start)
        .unwrap_or(true);
    let before_end = params.end.map(|end| compare_to <= end).unwrap_or(true);
    after_start && before_end
}

/// Checks if a filename matches the requested file type and time range filters.
/// Shared between FileAccess and S3FileAccess.
fn matches_file_params(filename: &str, params: &FileParams) -> Result<bool, Error> {
    let file_pieces: Vec<&str> = filename.split('_').collect();
    let Some(last_piece) = file_pieces.last() else {
        return Ok(false);
    };
    let created_time = drop_suffix(last_piece, ".parquet");
    let file_generated_at = OffsetDateTime::parse(&created_time, &Rfc3339)?;
    let valid_time_range = is_time_in_range(file_generated_at, params);
    let Some(file_data_type) = file_pieces.first() else {
        return Ok(false);
    };

    if let Some(true) = params.observations {
        if *file_data_type == "observations" && valid_time_range {
            return Ok(true);
        }
    }

    if let Some(true) = params.forecasts {
        if *file_data_type == "forecasts" && valid_time_range {
            return Ok(true);
        }
    }

    if params.forecasts.is_none() && params.observations.is_none() && valid_time_range {
        return Ok(true);
    }

    Ok(false)
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
    fn s3_key(filename: &str, file_generated_at: OffsetDateTime) -> String {
        format!("weather_data/{}/{}", file_generated_at.date(), filename)
    }
}

#[async_trait]
impl FileData for S3FileAccess {
    async fn grab_file_names(&self, params: FileParams) -> Result<Vec<String>, Error> {
        let mut file_names = Vec::new();

        // Determine the date range to scan
        let start_date = params.start.map(|s| s.date());
        let end_date = params.end.map(|e| e.date());

        // If we have a date range, list objects per date prefix for efficiency.
        // Otherwise, list everything under weather_data/.
        let prefixes: Vec<String> = if let (Some(start), Some(end)) = (start_date, end_date) {
            let mut dates = Vec::new();
            let mut current = start;
            while current <= end {
                dates.push(format!("weather_data/{}/", current));
                current = current.next_day().unwrap_or(end);
                if dates.len() > 365 {
                    break; // safety limit
                }
            }
            dates
        } else {
            vec!["weather_data/".to_string()]
        };

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
                        "S3 list_objects_v2 failed for prefix '{}': {}",
                        prefix, e
                    ))
                })?;

                for obj in resp.contents() {
                    if let Some(key) = obj.key() {
                        // Extract filename from key: weather_data/2026-02-16/forecasts_2026-02-16T10:00:00Z.parquet
                        if let Some(filename) = key.rsplit('/').next() {
                            if filename.ends_with(".parquet") {
                                if matches_file_params(filename, &params)? {
                                    file_names.push(filename.to_string());
                                }
                            }
                        }
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

    fn current_folder(&self) -> String {
        let current_date = OffsetDateTime::now_utc().date();
        format!("weather_data/{}", current_date)
    }

    fn build_file_paths(&self, file_names: Vec<String>) -> Vec<String> {
        file_names
            .iter()
            .map(|file_name| {
                let file_pieces: Vec<String> = file_name.split('_').map(|f| f.to_owned()).collect();
                let created_time = drop_suffix(file_pieces.last().unwrap(), ".parquet");
                let file_generated_at = OffsetDateTime::parse(&created_time, &Rfc3339).unwrap();
                format!("weather_data/{}/{}", file_generated_at.date(), file_name)
            })
            .collect()
    }

    fn build_file_path(&self, filename: &str, file_generated_at: OffsetDateTime) -> String {
        Self::s3_key(filename, file_generated_at)
    }

    async fn download_file(
        &self,
        filename: &str,
        file_generated_at: OffsetDateTime,
    ) -> Result<Body, Error> {
        let key = Self::s3_key(filename, file_generated_at);
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| Error::NotFound(format!("S3 get_object '{}': {}", key, e)))?;

        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| Error::Io(format!("S3 read body '{}': {}", key, e)))?
            .into_bytes();

        Ok(Body::from(bytes))
    }
}
