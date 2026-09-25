//! Query-ready copies of forecast files.
//!
//! A published forecast file holds about 610,000 rows for 3,900 stations,
//! with times as RFC 3339 strings and row groups that each mix hundreds of
//! stations, so a query for a few stations reads and parses every row of
//! every file it covers. A copy holds the same rows with times as instants,
//! temperatures in Fahrenheit and its publication recorded, sorted by
//! station into small row groups, so a station's rows are one row group of
//! each file.
//!
//! Queries read a copy where one exists and the published file otherwise,
//! through the same conversion, so results never depend on which copies
//! exist. A file whose columns have other types, or whose times do not
//! parse, gets no copy and is always read directly. Published files never
//! change, so a copy never goes stale.
//!
//! Copies live in `<weather dir>/derived/<VERSION>/<date>/<file name>`,
//! which file listings skip because `derived` is not a date.

use super::{Error, FORECAST_ROW_COLUMNS, open_connection, source_forecast_rows_sql};
use crate::file_access::ParquetFileName;
use log::{info, warn};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use time::Date;

/// Names the copies' layout. Change it whenever their columns, types or
/// order change; copies of other versions are then ignored and deleted.
pub(super) const VERSION: &str = "forecasts-v1";

/// Rows per row group: about 26 stations' periods, so a query for one
/// station reads one or two groups of each file.
const ROW_GROUP_SIZE: usize = 4096;

/// Every copy has these columns with these types, which is what makes the
/// copies of different files one table.
const COLUMN_TYPES: &[(&str, &str)] = &[
    ("station_id", "VARCHAR"),
    ("begin_ts", "TIMESTAMP WITH TIME ZONE"),
    ("end_ts", "TIMESTAMP WITH TIME ZONE"),
    ("generated_ts", "TIMESTAMP WITH TIME ZONE"),
    ("published_ts", "TIMESTAMP WITH TIME ZONE"),
    ("source", "VARCHAR"),
    ("min_temp", "DOUBLE"),
    ("max_temp", "DOUBLE"),
    ("wind_speed", "BIGINT"),
    ("wind_direction", "BIGINT"),
    ("relative_humidity_max", "BIGINT"),
    ("relative_humidity_min", "BIGINT"),
    ("precip_chance", "DOUBLE"),
    ("liquid_precipitation_amt", "DOUBLE"),
    ("snow_amt", "DOUBLE"),
    ("snow_ratio", "DOUBLE"),
    ("ice_amt", "DOUBLE"),
];

/// What happened when copying one file.
#[derive(Debug, PartialEq, Eq)]
pub enum Copied {
    Made,
    /// The file is read directly from now on.
    Refused,
}

#[derive(Clone, Debug)]
pub struct DerivedForecasts {
    parent: PathBuf,
    root: PathBuf,
}

impl DerivedForecasts {
    /// Copies kept under `directory`, e.g. `<weather dir>/derived`.
    pub fn new(directory: &Path) -> Self {
        Self {
            parent: directory.to_path_buf(),
            root: directory.join(VERSION),
        }
    }

    fn day_directory(&self, file: &ParquetFileName) -> PathBuf {
        self.root.join(file.generated_at.date().to_string())
    }

    fn path(&self, file: &ParquetFileName) -> PathBuf {
        self.day_directory(file).join(file.to_string())
    }

    fn refusal_marker(&self, file: &ParquetFileName) -> PathBuf {
        self.day_directory(file).join(format!("{file}.refused"))
    }

    /// The copy of `file`, if one has been made.
    pub fn existing(&self, file: &ParquetFileName) -> Option<String> {
        let path = self.path(file);
        path.is_file().then(|| path.to_string_lossy().into_owned())
    }

    /// Whether `file` still needs a copy: none was made or refused.
    pub fn missing(&self, file: &ParquetFileName) -> bool {
        !self.path(file).exists() && !self.refusal_marker(file).exists()
    }

    /// Copies the published forecast file at `source`. Blocks; run it off
    /// the async runtime.
    pub fn copy(&self, file: &ParquetFileName, source: &str) -> Result<Copied, Error> {
        let directory = self.day_directory(file);
        fs::create_dir_all(&directory)?;
        let connection = open_connection()?;
        let rows = source_forecast_rows_sql(&[source.to_owned()], "");

        let mut statement = connection.prepare(&format!(
            "DESCRIBE SELECT {FORECAST_ROW_COLUMNS} FROM ({rows})"
        ))?;
        let types = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let expected: Vec<(String, String)> = COLUMN_TYPES
            .iter()
            .map(|(name, kind)| ((*name).to_owned(), (*kind).to_owned()))
            .collect();
        if types != expected {
            warn!("reading {file} directly: its columns are {types:?}");
            return self.refuse(file);
        }

        let temporary = directory.join(format!(".{file}.{}.tmp", uuid::Uuid::now_v7()));
        let copy = format!(
            "COPY (SELECT {FORECAST_ROW_COLUMNS} FROM ({rows})
                   ORDER BY station_id, begin_ts, end_ts, generated_ts)
             TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE {ROW_GROUP_SIZE})",
            temporary.to_string_lossy().replace('\'', "''")
        );
        match connection.execute_batch(&copy) {
            Ok(()) => {
                // Another oracle process may have made the same copy; both
                // are identical, and the rename replaces atomically.
                fs::rename(&temporary, self.path(file))?;
                Ok(Copied::Made)
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                let message = error.to_string();
                if message.contains("Conversion Error") || message.contains("Invalid Input Error") {
                    warn!("reading {file} directly: {message}");
                    self.refuse(file)
                } else {
                    Err(error.into())
                }
            }
        }
    }

    fn refuse(&self, file: &ParquetFileName) -> Result<Copied, Error> {
        fs::write(self.refusal_marker(file), b"")?;
        Ok(Copied::Refused)
    }

    /// Deletes copies of files published before `oldest`, copies of other
    /// versions, and temporary files of interrupted copies older than an
    /// hour.
    pub fn prune(&self, oldest: Date) -> io::Result<()> {
        let Ok(versions) = fs::read_dir(&self.parent) else {
            return Ok(());
        };
        for version in versions.flatten() {
            let path = version.path();
            let copies = version
                .file_name()
                .to_string_lossy()
                .starts_with("forecasts-");
            if copies && path != self.root && version.file_type()?.is_dir() {
                info!("deleting derived forecasts {}", path.display());
                fs::remove_dir_all(path)?;
            }
        }
        let Ok(days) = fs::read_dir(&self.root) else {
            return Ok(());
        };
        let format = time::macros::format_description!("[year]-[month]-[day]");
        for day in days.flatten() {
            let old = day
                .file_name()
                .to_str()
                .and_then(|name| Date::parse(name, format).ok())
                .is_none_or(|date| date < oldest);
            if old {
                fs::remove_dir_all(day.path())?;
                continue;
            }
            for entry in fs::read_dir(day.path())?.flatten() {
                let abandoned = entry.file_name().to_string_lossy().ends_with(".tmp")
                    && entry
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age.as_secs() > 60 * 60);
                if abandoned {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }
}
