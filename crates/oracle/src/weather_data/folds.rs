//! Files holding, for each station and forecast period, the row a query
//! would pick from the forecast files published in one span of time: a UTC
//! day, or a quarter of one (six hours).
//!
//! A forecast query picks, per station and period, the row of the newest
//! issue, then the newest publication, then the largest values (see
//! [`super::DEDUPE_ORDER`]). That is the maximum under one total order, so
//! it can be taken in stages: first over a span's files, then over those
//! results and any other rows. A span's fold therefore stands in for its
//! files in any query that reads every row of every one of them: each file
//! was published inside the query's publication window, and all its issue
//! times lie inside the query's issue window. Otherwise the query reads the
//! files themselves, so results never depend on which folds exist.
//!
//! A query uses a day's fold where it can, else the folds of the day's
//! quarters, else single files. A week of issues is then about eight
//! small files instead of 170 large ones, and a reader's local day, which
//! starts at some hour of a UTC day, needs at most five hours of single
//! files at either end. A fold keeps about one row in twenty of its files,
//! sorted by station.
//!
//! Folds are written from the derived copies (see [`super::derived`]) and
//! extended as each upload arrives. `<span>.json` names a span's fold file
//! and the files folded into it with their issue times. Both are replaced
//! by renaming, so a reader sees a complete fold or the previous one. A
//! replaced fold file is deleted ten minutes later, after any query that
//! read the previous manifest has finished.

use super::{DEDUPE_ORDER, Error, FORECAST_ROW_COLUMNS, open_connection, sql_string_list};
use crate::file_access::ParquetFileName;
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use time::{Date, OffsetDateTime};

/// Names the folds' layout. Change it whenever their columns, types or
/// order change; folds of other versions are then deleted.
const VERSION: &str = "folds-v1";

/// Rows per row group, as in the copies: a station's periods of one span
/// are one or two groups.
const ROW_GROUP_SIZE: usize = 4096;

/// How long a replaced fold file stays readable.
const RETIRED: Duration = Duration::from_secs(10 * 60);

/// Stations folded at a time. The grouping holds a row per station and
/// period, about 200 a day per station, so a batch stays well inside the
/// connection's memory limit where a whole day of 3,900 stations does not.
const STATIONS_PER_BATCH: usize = 512;

/// The publication times folded together: a day, or its quarter
/// `0..4` (hours `6 * quarter` up to `6 * quarter + 6`), both in the
/// offset of the file names, which the daemon writes in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Span {
    day: Date,
    quarter: Option<u8>,
}

impl Span {
    fn day(file: &ParquetFileName) -> Self {
        Self {
            day: file.generated_at.date(),
            quarter: None,
        }
    }

    fn quarter(file: &ParquetFileName) -> Self {
        Self {
            day: file.generated_at.date(),
            quarter: Some(file.generated_at.hour() / 6),
        }
    }

    /// `2026-09-23` for a day, `2026-09-23T06` for its second quarter.
    fn name(&self) -> String {
        match self.quarter {
            None => self.day.to_string(),
            Some(quarter) => format!("{}T{:02}", self.day, quarter * 6),
        }
    }
}

/// A folded file and the range of issue times in it; none for a file
/// without issue times, whose rows no query reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Source {
    name: String,
    #[serde(with = "time::serde::rfc3339::option")]
    first_issue: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    last_issue: Option<OffsetDateTime>,
}

impl Source {
    /// Every row of the file was issued in `from..=to`.
    fn inside(&self, (from, to): (OffsetDateTime, OffsetDateTime)) -> bool {
        self.first_issue.is_none_or(|first| from <= first)
            && self.last_issue.is_none_or(|last| last <= to)
    }

    /// No row of the file was issued in `from..=to`.
    fn outside(&self, (from, to): (OffsetDateTime, OffsetDateTime)) -> bool {
        match (self.first_issue, self.last_issue) {
            (Some(first), Some(last)) => last < from || to < first,
            _ => true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    file: String,
    /// Sorted by name.
    sources: Vec<Source>,
}

/// What a forecast query reads: folds, and single files by name.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Plan {
    pub folds: Vec<String>,
    pub files: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Folds {
    parent: PathBuf,
    root: PathBuf,
}

impl Folds {
    /// Folds kept under `directory`, e.g. `<weather dir>/derived`.
    pub fn new(directory: &Path) -> Self {
        Self {
            parent: directory.to_path_buf(),
            root: directory.join(VERSION),
        }
    }

    fn manifest_path(&self, span: Span) -> PathBuf {
        self.root.join(format!("{}.json", span.name()))
    }

    /// The span's manifest, if its fold file exists.
    fn manifest(&self, span: Span) -> Option<Manifest> {
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(self.manifest_path(span)).ok()?).ok()?;
        let valid = manifest.file.starts_with(&format!("{}-", span.name()))
            && manifest.file.ends_with(".parquet")
            && manifest
                .file
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
            && self.root.join(&manifest.file).is_file();
        valid.then_some(manifest)
    }

    /// Which folds and single files hold the rows of the forecast files
    /// `names` (those published in the query's publication window) that
    /// were issued in `issued`. Files whose rows were all issued outside
    /// that window are left out when a manifest says so.
    pub(super) fn plan(
        &self,
        names: Vec<String>,
        (issued_from, issued_to): (OffsetDateTime, OffsetDateTime),
    ) -> Plan {
        // Copies and folds record a name as the file access writes it.
        let mut days = BTreeMap::<Span, Vec<ParquetFileName>>::new();
        for name in names {
            if let Ok(file) = ParquetFileName::parse(&name) {
                days.entry(Span::day(&file)).or_default().push(file);
            }
        }
        let window = (issued_from, issued_to);
        let mut plan = Plan::default();
        for (day, files) in days {
            let mut known = HashMap::new();
            let rest = self.use_fold(day, files, window, &mut plan, &mut known);
            let mut quarters = BTreeMap::<Span, Vec<ParquetFileName>>::new();
            for file in rest {
                quarters.entry(Span::quarter(&file)).or_default().push(file);
            }
            for (quarter, files) in quarters {
                for file in self.use_fold(quarter, files, window, &mut plan, &mut known) {
                    let name = file.to_string();
                    if !known
                        .get(&name)
                        .is_some_and(|source: &Source| source.outside(window))
                    {
                        plan.files.push(name);
                    }
                }
            }
        }
        plan
    }

    /// Adds `span`'s fold to `plan` if every file in it is wholly inside
    /// the query, and returns the `listed` files it does not cover. Records
    /// the files its manifest knows in `known`.
    fn use_fold(
        &self,
        span: Span,
        listed: Vec<ParquetFileName>,
        window: (OffsetDateTime, OffsetDateTime),
        plan: &mut Plan,
        known: &mut HashMap<String, Source>,
    ) -> Vec<ParquetFileName> {
        let Some(manifest) = self.manifest(span) else {
            return listed;
        };
        for source in &manifest.sources {
            known.insert(source.name.clone(), source.clone());
        }
        let names: HashSet<String> = listed.iter().map(ToString::to_string).collect();
        let inside = manifest
            .sources
            .iter()
            .all(|source| names.contains(&source.name) && source.inside(window));
        if !inside {
            return listed;
        }
        plan.folds.push(
            self.root
                .join(&manifest.file)
                .to_string_lossy()
                .into_owned(),
        );
        let folded: HashSet<&str> = manifest
            .sources
            .iter()
            .map(|source| source.name.as_str())
            .collect();
        listed
            .into_iter()
            .filter(|file| !folded.contains(file.to_string().as_str()))
            .collect()
    }

    /// Folds each day and each quarter of the copied forecast files,
    /// `(file, copy path)`, extending current folds where they cover some
    /// of them. Returns how many folds were written. Blocks; run it off the
    /// async runtime.
    pub(super) fn fold_all(&self, copies: &[(ParquetFileName, String)]) -> Result<usize, Error> {
        let mut spans = BTreeMap::<Span, Vec<(String, String)>>::new();
        for (file, copy) in copies {
            for span in [Span::day(file), Span::quarter(file)] {
                spans
                    .entry(span)
                    .or_default()
                    .push((file.to_string(), copy.clone()));
            }
        }
        let mut written = 0;
        // Newest first: current pages read the newest files.
        for (span, copies) in spans.into_iter().rev() {
            if self.fold(span, copies)? {
                written += 1;
            }
        }
        Ok(written)
    }

    /// Folds a span's copied files, `(name, copy path)`. Returns whether a
    /// new fold was written.
    fn fold(&self, span: Span, mut copies: Vec<(String, String)>) -> Result<bool, Error> {
        copies.sort();
        copies.dedup();
        let current = self.manifest(span).filter(|manifest| {
            manifest
                .sources
                .iter()
                .all(|source| copies.iter().any(|(name, _)| *name == source.name))
        });
        let folded: HashSet<&str> = current
            .iter()
            .flat_map(|manifest| manifest.sources.iter().map(|source| source.name.as_str()))
            .collect();
        let added: Vec<_> = copies
            .iter()
            .filter(|(name, _)| !folded.contains(name.as_str()))
            .collect();
        if added.is_empty() {
            return Ok(false);
        }
        fs::create_dir_all(&self.root)?;
        let connection = open_connection()?;
        let added_paths: Vec<String> = added.iter().map(|(_, path)| path.clone()).collect();

        // Issue times of the added files, by the `<date>/<name>` each row
        // records as its source.
        let mut statement = connection.prepare(&format!(
            "SELECT source, epoch_ns(min(generated_ts)), epoch_ns(max(generated_ts))
             FROM read_parquet([{}]) GROUP BY source",
            sql_string_list(&added_paths)
        ))?;
        let ranges = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let at = |nanos: Option<i64>| {
            nanos
                .map(|nanos| OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos)))
                .transpose()
                .map_err(|error| io::Error::other(error.to_string()))
        };
        let mut sources = current
            .as_ref()
            .map(|manifest| manifest.sources.clone())
            .unwrap_or_default();
        for (name, _) in &added {
            let (first, last) = ranges
                .iter()
                .find(|(source, _, _)| {
                    source
                        .as_deref()
                        .and_then(|source| source.rsplit('/').next())
                        == Some(name.as_str())
                })
                .map(|(_, first, last)| (*first, *last))
                .unwrap_or_default();
            sources.push(Source {
                name: name.clone(),
                first_issue: at(first)?,
                last_issue: at(last)?,
            });
        }
        sources.sort_by(|a, b| a.name.cmp(&b.name));

        let mut inputs = added_paths;
        if let Some(manifest) = &current {
            inputs.push(
                self.root
                    .join(&manifest.file)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        let key: String = Sha256::digest(
            sources
                .iter()
                .map(|source| source.name.as_str())
                .collect::<Vec<_>>()
                .join("\n")
                .as_bytes(),
        )
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
        let file = format!("{}-{key}.parquet", span.name());
        let scratch = self
            .root
            .join(format!(".{file}.{}.tmp", uuid::Uuid::now_v7()));
        let folded = fold_rows(&connection, &inputs, &scratch).and_then(|()| {
            // Another oracle process may have written the same fold; both
            // are identical, and the rename replaces atomically.
            fs::rename(scratch.join("fold.parquet"), self.root.join(&file))?;
            Ok(())
        });
        drop(connection);
        let _ = fs::remove_dir_all(&scratch);
        folded?;

        let manifest = Manifest { file, sources };
        let temporary = self.root.join(format!(
            ".{}.json.{}.tmp",
            span.name(),
            uuid::Uuid::now_v7()
        ));
        fs::write(
            &temporary,
            serde_json::to_vec(&manifest).map_err(io::Error::other)?,
        )?;
        fs::rename(&temporary, self.manifest_path(span))?;
        // A query may have just read the previous manifest: start the
        // previous file's retirement now, however old it is.
        if let Some(previous) = current
            && previous.file != manifest.file
        {
            let _ = fs::File::open(self.root.join(previous.file))
                .and_then(|file| file.set_modified(SystemTime::now()));
        }
        Ok(true)
    }

    /// Deletes folds of days before `oldest`, folds of other versions,
    /// retired fold files and temporary files of interrupted folds.
    pub fn prune(&self, oldest: Date) -> io::Result<()> {
        if let Ok(versions) = fs::read_dir(&self.parent) {
            for version in versions.flatten() {
                let folds = version.file_name().to_string_lossy().starts_with("folds-");
                if folds && version.path() != self.root && version.file_type()?.is_dir() {
                    fs::remove_dir_all(version.path())?;
                }
            }
        }
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Ok(());
        };
        let format = time::macros::format_description!("[year]-[month]-[day]");
        let day_of = |name: &str| name.get(..10).and_then(|day| Date::parse(day, format).ok());
        let mut current = HashSet::new();
        let mut files = vec![];
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json") && !name.starts_with('.') {
                if day_of(&name).is_none_or(|day| day < oldest) {
                    fs::remove_file(entry.path())?;
                } else if let Ok(bytes) = fs::read(entry.path())
                    && let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes)
                {
                    current.insert(manifest.file);
                }
            } else {
                files.push((name, entry));
            }
        }
        for (name, entry) in files {
            let age = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .unwrap_or_default();
            let fold = name.ends_with(".parquet");
            let old_day = fold && day_of(&name).is_none_or(|day| day < oldest);
            let retired = fold && !current.contains(&name) && age > RETIRED;
            let abandoned = name.ends_with(".tmp") && age > Duration::from_secs(60 * 60);
            if old_day || retired {
                let _ = fs::remove_file(entry.path());
            } else if abandoned {
                let _ = fs::remove_dir_all(entry.path()).or_else(|_| fs::remove_file(entry.path()));
            }
        }
        Ok(())
    }
}

/// Writes `<scratch>/fold.parquet`: for each station and period in the
/// forecast rows of `inputs` (copies or folds, which share their columns),
/// the row a query picks, sorted by station and period. Stations are folded
/// in batches into part files, which are then joined in order.
fn fold_rows(connection: &Connection, inputs: &[String], scratch: &Path) -> Result<(), Error> {
    fs::create_dir_all(scratch)?;
    let inputs = sql_string_list(inputs);
    let mut statement = connection.prepare(&format!(
        "SELECT DISTINCT station_id FROM read_parquet([{inputs}])
         WHERE station_id IS NOT NULL ORDER BY station_id"
    ))?;
    let stations = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let quoted = |station: &str| format!("'{}'", station.replace('\'', "''"));
    let mut batches: Vec<String> = stations
        .chunks(STATIONS_PER_BATCH)
        .map(|batch| {
            format!(
                "station_id BETWEEN {} AND {}",
                quoted(&batch[0]),
                quoted(&batch[batch.len() - 1])
            )
        })
        .collect();
    // Rows without a station stay in the fold, as they stay in queries.
    batches.push("station_id IS NULL".to_owned());
    let mut parts = vec![];
    for (index, stations) in batches.iter().enumerate() {
        let part = scratch.join(format!("part-{index:05}.parquet"));
        connection.execute_batch(&format!(
            "COPY (
                SELECT station_id, begin_ts, end_ts, picked.*
                FROM (
                    SELECT station_id, begin_ts, end_ts,
                        FIRST(STRUCT_PACK(generated_ts, published_ts, source, min_temp, max_temp,
                            wind_speed, wind_direction, relative_humidity_max, relative_humidity_min,
                            precip_chance, liquid_precipitation_amt, snow_amt, snow_ratio, ice_amt)
                            ORDER BY {DEDUPE_ORDER}) AS picked
                    FROM (SELECT {FORECAST_ROW_COLUMNS} FROM read_parquet([{inputs}]) WHERE {stations})
                    GROUP BY station_id, begin_ts, end_ts
                )
                ORDER BY station_id, begin_ts, end_ts
             ) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE {ROW_GROUP_SIZE})",
            part.to_string_lossy().replace('\'', "''")
        ))?;
        parts.push(part.to_string_lossy().into_owned());
    }
    // Read in order, the parts keep the fold sorted by station.
    connection.execute_batch(&format!(
        "SET preserve_insertion_order = true;
         COPY (SELECT * FROM read_parquet([{}]))
         TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE {ROW_GROUP_SIZE})",
        sql_string_list(&parts),
        scratch
            .join("fold.parquet")
            .to_string_lossy()
            .replace('\'', "''")
    ))?;
    Ok(())
}
