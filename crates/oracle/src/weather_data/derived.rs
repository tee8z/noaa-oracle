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
    time::{Duration, SystemTime},
};
use time::Date;

/// Names the copies' layout. Change it whenever their columns, types or
/// order change; copies of other versions are then ignored, and deleted
/// once idle (see [`prune_other_versions`]).
pub(super) const VERSION: &str = "forecasts-v2-native-intervals";

/// How long another version's copies or folds go untouched before they are
/// deleted. A running oracle touches its own version's directories on every
/// preparation pass, at least every ten minutes, so two versions can serve
/// side by side during a blue/green deploy without deleting each other's.
const OTHER_VERSION_IDLE: Duration = Duration::from_secs(6 * 60 * 60);

/// Marks `own` as in use, then deletes the directories under `parent` whose
/// names start with `prefix`, other than `own`, that no oracle has touched
/// for [`OTHER_VERSION_IDLE`].
pub(super) fn prune_other_versions(parent: &Path, prefix: &str, own: &Path) -> io::Result<()> {
    if own.is_dir()
        && let Err(error) = fs::File::open(own).and_then(|dir| dir.set_modified(SystemTime::now()))
    {
        warn!("cannot mark {} in use: {error}", own.display());
    }
    let Ok(versions) = fs::read_dir(parent) else {
        return Ok(());
    };
    for version in versions.flatten() {
        let path = version.path();
        if path == own
            || !version.file_name().to_string_lossy().starts_with(prefix)
            || !version.file_type()?.is_dir()
        {
            continue;
        }
        let idle = version
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= OTHER_VERSION_IDLE);
        if idle {
            info!("deleting {} of another version", path.display());
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

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
    ("forecast_interval_version", "VARCHAR"),
    ("interval_kind", "VARCHAR"),
    ("source_url", "VARCHAR"),
    ("source_received_at", "VARCHAR"),
    ("source_xml_sha256", "VARCHAR"),
    ("source_location", "VARCHAR"),
    ("source_layouts", "VARCHAR"),
    ("quality_status", "VARCHAR"),
    ("quality_reason", "VARCHAR"),
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

    /// Deletes copies of files published before `oldest`, idle copies of
    /// other versions, and temporary files of interrupted copies older than
    /// an hour.
    pub fn prune(&self, oldest: Date) -> io::Result<()> {
        prune_other_versions(&self.parent, "forecasts-", &self.root)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn age(path: &Path, by: Duration) {
        fs::File::open(path)
            .and_then(|dir| dir.set_modified(SystemTime::now() - by))
            .unwrap();
    }

    /// During a blue/green deploy between versions, each oracle keeps the
    /// other's copies; a version no oracle runs any more goes once idle.
    #[test]
    fn other_versions_are_deleted_only_once_idle() {
        let parent = tempfile::tempdir().unwrap();
        let own = parent.path().join("forecasts-v3");
        let serving = parent.path().join("forecasts-v2");
        let abandoned = parent.path().join("forecasts-v1");
        let folds = parent.path().join("folds-v1");
        for dir in [&own, &serving, &abandoned, &folds] {
            fs::create_dir_all(dir.join("2026-09-27")).unwrap();
        }
        age(&own, OTHER_VERSION_IDLE * 2);
        age(&serving, OTHER_VERSION_IDLE / 2);
        age(&abandoned, OTHER_VERSION_IDLE + Duration::from_secs(60));
        age(&folds, OTHER_VERSION_IDLE * 2);

        prune_other_versions(parent.path(), "forecasts-", &own).unwrap();

        assert!(own.exists() && serving.exists());
        assert!(!abandoned.exists());
        assert!(
            folds.exists(),
            "another cache's prefix is left to its own prune"
        );
        // Marked in use, so another version keeps it.
        let idle = fs::metadata(&own)
            .and_then(|metadata| metadata.modified())
            .unwrap()
            .elapsed()
            .unwrap();
        assert!(idle < Duration::from_secs(60));
    }
}
