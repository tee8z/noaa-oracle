//! Query-ready copies of forecast files.
//!
//! A published forecast file holds about 660,000 rows for 4,000 stations,
//! with times as RFC 3339 strings and row groups that each mix hundreds of
//! stations, so a query for a few stations reads and parses every file it
//! covers. A copy holds the same rows with times as instants, temperatures
//! in Fahrenheit and its publication recorded, sorted by station into small
//! row groups. Daily compaction puts each station's publications together in
//! buckets by its first two characters, with larger groups to reduce footers.
//! A station query opens only its bucket for each day, rather than all snapshots. Every
//! original row and its publication identity are retained. Queries restrict
//! compacted rows to their exact selected source files.
//!
//! Queries read a copy where one exists and the published file otherwise,
//! through the same conversion, so results never depend on which copies
//! exist. A file whose columns have other types, or whose times do not
//! parse, gets no copy and is always read directly. Published files never
//! change, so a copy never goes stale.
//!
//! Copies and atomic compaction manifests live in
//! `<weather dir>/derived/<VERSION>/<date>/`, which file
//! listings skip because `derived` is not a date.

use super::{
    Error, FORECAST_ROW_COLUMNS, open_connection, source_forecast_rows_sql, sql_string_list,
};
use crate::file_access::ParquetFileName;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use time::Date;

/// Names the copies' layout. Change it whenever their columns, types or
/// order change; copies of other versions are then ignored and deleted.
pub(super) const VERSION: &str = "forecasts-v1";

/// Rows per row group: about 24 stations' periods, so a query for one
/// station reads one or two groups of each file.
const ROW_GROUP_SIZE: usize = 4096;

/// Temporary files older than this belong to an interrupted copy.
const ABANDONED: Duration = Duration::from_secs(60 * 60);

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

/// Immutable daily station buckets and the original publications they contain. Keeping
/// every row lets queries retain exact issue and publication cutoffs.
#[derive(Serialize, Deserialize)]
struct CompactManifest {
    key: String,
    sources: Vec<String>,
    partitions: Vec<String>,
}

#[derive(Debug)]
pub(super) struct CompactSelection {
    pub paths: Vec<String>,
    pub sources: Vec<String>,
}

impl CompactManifest {
    fn entry_name(&self) -> String {
        format!(".compact-{}", self.key)
    }
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

    fn manifest(&self, day: Date) -> Option<CompactManifest> {
        let bytes = fs::read(self.root.join(day.to_string()).join(".compact.json")).ok()?;
        let manifest: CompactManifest = serde_json::from_slice(&bytes).ok()?;
        (manifest.key.len() == 64
            && manifest.key.bytes().all(|b| b.is_ascii_hexdigit())
            && manifest.partitions.iter().all(|partition| {
                partition.strip_prefix("bucket=").is_some_and(|value| {
                    !value.is_empty()
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
            }))
        .then_some(manifest)
    }

    /// Daily copies covering selected originals, and the original names covered.
    /// New uploads that are absent from the manifest still read their own copy.
    pub(super) fn compact_files(
        &self,
        names: &[String],
        stations: &[String],
    ) -> (Vec<CompactSelection>, HashSet<String>) {
        let wanted: HashSet<String> = stations
            .iter()
            .map(|station| {
                let prefix: String = station
                    .bytes()
                    .take(2)
                    .map(|byte| format!("{byte:02X}"))
                    .collect();
                format!("bucket={prefix}")
            })
            .collect();
        let mut days = BTreeMap::<Date, Vec<&String>>::new();
        for name in names {
            if let Ok(file) = ParquetFileName::parse(name) {
                days.entry(file.generated_at.date()).or_default().push(name);
            }
        }
        let mut compacted = Vec::new();
        let mut covered = HashSet::new();
        for (day, names) in days {
            let Some(manifest) = self.manifest(day) else {
                continue;
            };
            let path = self.root.join(day.to_string()).join(manifest.entry_name());
            if !path.is_dir() {
                continue;
            }
            let paths: Vec<String> = manifest
                .partitions
                .iter()
                .filter(|partition| wanted.is_empty() || wanted.contains(*partition))
                .map(|partition| {
                    path.join(partition)
                        .join("*.parquet")
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            // Missing station buckets contain no matching rows. Covered source
            // names are still excluded from the per-file fallback below.
            let available: HashSet<_> = manifest.sources.iter().collect();
            let mut sources = Vec::new();
            for name in names {
                if available.contains(name) {
                    sources.push(format!("{day}/{name}"));
                    covered.insert(name.clone());
                }
            }
            if !sources.is_empty() && !paths.is_empty() {
                compacted.push(CompactSelection { paths, sources });
            }
        }
        (compacted, covered)
    }

    /// Repack a day's ready copies into station-sorted buckets. The manifest is
    /// switched only after every bucket exists, so requests keep using either
    /// the complete old copy plus uncovered publications, or the complete new one.
    pub(super) fn compact_day(&self, files: &[ParquetFileName]) -> Result<bool, Error> {
        let Some(first) = files.first() else {
            return Ok(false);
        };
        let day = first.generated_at.date();
        let mut sources: Vec<_> = files
            .iter()
            .filter(|file| file.generated_at.date() == day && self.existing(file).is_some())
            .map(ToString::to_string)
            .collect();
        sources.sort();
        sources.dedup();
        if sources.len() < 2 {
            return Ok(false);
        }
        let key: String =
            Sha256::digest(format!("station-buckets-v1-8192\n{}", sources.join("\n")).as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
        if self.manifest(day).is_some_and(|manifest| {
            manifest.key == key
                && self
                    .root
                    .join(day.to_string())
                    .join(manifest.entry_name())
                    .is_dir()
        }) {
            return Ok(false);
        }
        let paths: Vec<_> = sources
            .iter()
            .map(|name| {
                self.root
                    .join(day.to_string())
                    .join(name)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let mut manifest = CompactManifest {
            key,
            sources,
            partitions: vec![],
        };
        let directory = self.root.join(day.to_string());
        let temporary = directory.join(format!(".compact-{}.tmp", uuid::Uuid::now_v7()));
        let connection = open_connection()?;
        // The explicit sort provides clustering; parallel order-preserving COPY
        // buffers can otherwise exceed the same memory limit on a whole day.
        let scratch = directory.join(format!(".compact-spill-{}.tmp", uuid::Uuid::now_v7()));
        fs::create_dir_all(&scratch)?;
        connection.execute_batch(&format!(
            "SET threads = 1; SET preserve_insertion_order = false;
             SET partitioned_write_flush_threshold = 8192;
             SET partitioned_write_max_open_files = 32;
             SET temp_directory = '{}'; SET max_temp_directory_size = '4GB';",
            scratch.to_string_lossy().replace('\'', "''")
        ))?;
        let sql = format!(
            "COPY (SELECT {FORECAST_ROW_COLUMNS}, hex(left(station_id, 2)) AS bucket FROM read_parquet([{}])
                   ORDER BY station_id, begin_ts, end_ts, generated_ts, published_ts, source)
             TO '{}' (FORMAT PARQUET, PARTITION_BY (bucket), COMPRESSION ZSTD, ROW_GROUP_SIZE 8192)",
            sql_string_list(&paths),
            temporary.to_string_lossy().replace('\'', "''")
        );
        let copied = connection.execute_batch(&sql);
        drop(connection);
        let _ = fs::remove_dir_all(scratch);
        if let Err(error) = copied {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error.into());
        }
        for entry in fs::read_dir(&temporary)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                manifest
                    .partitions
                    .push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        manifest.partitions.sort();
        let destination = directory.join(manifest.entry_name());
        if destination.is_dir() {
            // Another process already completed the same immutable generation.
            fs::remove_dir_all(&temporary)?;
        } else {
            fs::rename(&temporary, &destination)?;
        }
        let temporary = directory.join(format!(".compact-manifest-{}.tmp", uuid::Uuid::now_v7()));
        fs::write(
            &temporary,
            serde_json::to_vec(&manifest)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        )?;
        // A query may have just read the old manifest. Start its retirement
        // grace now, even when the old file itself was created days ago.
        if let Some(previous) = self.manifest(day) {
            let path = directory.join(previous.entry_name());
            if path.exists() {
                fs::File::open(path)?.set_modified(SystemTime::now())?;
            }
        }
        fs::rename(temporary, directory.join(".compact.json"))?;
        Ok(true)
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
    /// versions, and temporary files of interrupted copies.
    pub fn prune(&self, oldest: Date) -> io::Result<()> {
        let Ok(versions) = fs::read_dir(&self.parent) else {
            return Ok(());
        };
        for version in versions.flatten() {
            let forecasts = version
                .file_name()
                .to_string_lossy()
                .starts_with("forecasts-");
            if forecasts && version.path() != self.root && version.file_type()?.is_dir() {
                info!("deleting derived forecasts {}", version.path().display());
                fs::remove_dir_all(version.path())?;
            }
        }
        let Ok(days) = fs::read_dir(&self.root) else {
            return Ok(());
        };
        let format = time::macros::format_description!("[year]-[month]-[day]");
        for day in days.flatten() {
            let name = day.file_name();
            let old = name
                .to_str()
                .and_then(|name| Date::parse(name, format).ok())
                .is_none_or(|date| date < oldest);
            if old {
                fs::remove_dir_all(day.path())?;
                continue;
            }
            let current = fs::read(day.path().join(".compact.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<CompactManifest>(&bytes).ok())
                .map(|manifest| manifest.entry_name());
            for entry in fs::read_dir(day.path())?.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let obsolete = name.ends_with(".tmp")
                    || (name.starts_with(".compact-") && current.as_deref() != Some(name.as_str()));
                let abandoned = obsolete
                    && entry
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > ABANDONED);
                if abandoned {
                    if entry.file_type()?.is_dir() {
                        let _ = fs::remove_dir_all(entry.path());
                    } else {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
        Ok(())
    }
}
