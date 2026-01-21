use async_trait::async_trait;
use log::trace;
use serde::{Deserialize, Serialize};
use time::{
    format_description::well_known::Rfc3339, macros::format_description, Date, OffsetDateTime,
};
use tokio::fs;
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
}

#[async_trait]
pub trait FileData: Send + Sync {
    async fn grab_file_names(&self, params: FileParams) -> Result<Vec<String>, Error>;
    fn current_folder(&self) -> String;
    fn build_file_paths(&self, file_names: Vec<String>) -> Vec<String>;
    fn build_file_path(&self, filename: &str, file_generated_at: OffsetDateTime) -> String;
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
