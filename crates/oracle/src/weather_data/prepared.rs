//! Immutable native storage for current-day forecast queries. Every source row
//! overlapping the day is retained, including older issues and corrections.
//! Readers still apply their exact publication and generation bounds.

use super::{
    Error, FORECAST_ROW_COLUMNS, ForecastRequest, derived::DerivedForecasts, open_connection,
    sql_string_list,
};
use crate::file_access::ParquetFileName;
use log::info;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};
use time::{Date, Duration, OffsetDateTime, Time, format_description::well_known::Rfc3339};
use tokio_util::sync::CancellationToken;

const VERSION: &str = "prepared-current-v1";
const MAX_DATABASE_BYTES: u64 = 1024 * 1024 * 1024;
// Native database files are capped separately from the 4 GB temporary spill allowance.
const MAX_NATIVE_DATA_BYTES: u64 = 6 * MAX_DATABASE_BYTES;

#[derive(Clone, Debug)]
pub(super) struct PreparedForecasts {
    root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    key: String,
    file: String,
    #[serde(with = "time::serde::rfc3339")]
    start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    end: OffsetDateTime,
    sources: Vec<String>,
}

#[derive(Debug)]
pub(super) struct Generation {
    pub path: PathBuf,
    day: Date,
    // The lock follows the attached reader, including cancelled HTTP requests.
    // Cleanup in another process must acquire an exclusive lock before removal.
    _lock: fs::File,
}

#[derive(Debug)]
pub(super) struct Selection {
    pub generation: Arc<Generation>,
    pub sources: Vec<String>,
}

impl PreparedForecasts {
    pub fn new(parent: &Path) -> Self {
        Self {
            root: parent.join(VERSION),
        }
    }

    fn manifest_path(&self, day: Date) -> PathBuf {
        self.root.join(format!("{day}.json"))
    }

    fn manifest(&self, day: Date) -> Option<Manifest> {
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(self.manifest_path(day)).ok()?).ok()?;
        let valid = manifest.file.starts_with("generation-")
            && manifest.file.ends_with(".duckdb")
            && manifest
                .file
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
            && manifest.start == day.with_time(Time::MIDNIGHT).assume_utc()
            && manifest.end == manifest.start + Duration::days(1);
        valid.then_some(manifest)
    }

    pub fn select(
        &self,
        req: &ForecastRequest,
        names: &[String],
    ) -> Option<(Selection, HashSet<String>)> {
        let (start, end) = (req.start?, req.end?);
        let manifest = self.manifest(start.to_offset(time::UtcOffset::UTC).date())?;
        if start < manifest.start || end > manifest.end || end < start {
            return None;
        }
        let path = self.root.join(&manifest.file);
        if path.with_extension("bad").exists() {
            return None;
        }
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))
            .ok()?;
        lock.try_lock_shared().ok()?;
        // A cleanup may have won the race between reading the manifest and
        // acquiring this lock. UUID paths are never reused.
        if !path.is_file() {
            return None;
        }
        let available: HashSet<_> = manifest.sources.iter().collect();
        let mut sources = Vec::new();
        let mut covered = HashSet::new();
        for name in names {
            let file = ParquetFileName::parse(name).ok()?;
            let source = format!("{}/{name}", file.generated_at.date());
            if available.contains(&source) {
                covered.insert(name.clone());
                sources.push(source);
            }
        }
        if sources.is_empty() {
            return None;
        }
        Some((
            Selection {
                generation: Arc::new(Generation {
                    path,
                    day: manifest.start.date(),
                    _lock: lock,
                }),
                sources,
            },
            covered,
        ))
    }

    pub fn is_current(&self, generation: &Generation) -> bool {
        self.manifest(generation.day)
            .is_some_and(|manifest| self.root.join(manifest.file) == generation.path)
    }

    pub fn reject(&self, generation: &Generation) {
        let _ = fs::write(
            generation.path.with_extension("bad"),
            b"native reader failed; rebuild this generation\n",
        );
    }

    /// Build one UTC validity day, in station-prefix batches. A batch keeps
    /// sorting and native writer buffers bounded without sorting the full day.
    pub fn prepare_day(
        &self,
        day: Date,
        retention_day: Date,
        files: &[ParquetFileName],
        derived: &DerivedForecasts,
        stopping: &CancellationToken,
    ) -> Result<bool, Error> {
        fs::create_dir_all(&self.root)?;
        let prepare_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("prepare.lock"))?;
        if prepare_lock.try_lock().is_err() || stopping.is_cancelled() {
            return Ok(false);
        }
        // This exclusive builder lock proves no other live builder owns these
        // unfinished artifacts, including ones left after a process crash.
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("building-")
                || name.starts_with("manifest-")
                || name.starts_with("spill-")
            {
                if entry.file_type()?.is_dir() {
                    fs::remove_dir_all(entry.path())?;
                } else {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        let mut names: Vec<_> = files
            .iter()
            .filter(|file| derived.existing(file).is_some())
            .map(ToString::to_string)
            .collect();
        names.sort();
        names.dedup();
        if names.is_empty() {
            return Ok(false);
        }
        let key: String =
            Sha256::digest(format!("{VERSION}\n{day}\n{}", names.join("\n")).as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
        if self.manifest(day).is_some_and(|m| {
            m.key == key
                && self.root.join(&m.file).is_file()
                && !self.root.join(&m.file).with_extension("bad").exists()
        }) {
            return Ok(false);
        }
        self.prune(retention_day)?;
        let used: u64 = fs::read_dir(&self.root)?
            .map(|entry| entry.and_then(|e| e.metadata()))
            .collect::<io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum();
        if used + MAX_DATABASE_BYTES > MAX_NATIVE_DATA_BYTES {
            return Err(io::Error::other("prepared forecast storage budget is full").into());
        }

        let start = day.with_time(Time::MIDNIGHT).assume_utc();
        let end = start + Duration::days(1);
        let overlap = format!(
            "end_ts > '{}'::TIMESTAMPTZ AND begin_ts < '{}'::TIMESTAMPTZ",
            start.format(&Rfc3339)?,
            end.format(&Rfc3339)?
        );
        let (compacted, covered) = derived.compact_files(&names, &[]);
        let mut groups = BTreeMap::<String, Vec<String>>::new();
        for selected in compacted {
            for path in selected.paths {
                let prefix = Path::new(&path)
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix("bucket="))
                    .ok_or_else(|| io::Error::other("invalid prepared input bucket"))?;
                let prefix = if prefix == "__HIVE_DEFAULT_PARTITION__" {
                    "__NULL__"
                } else {
                    prefix
                };
                groups.entry(prefix.to_owned()).or_default().push(format!(
                    "SELECT {FORECAST_ROW_COLUMNS} FROM read_parquet([{}], hive_partitioning=false) WHERE {overlap} AND source IN ({})",
                    sql_string_list(&[path]), sql_string_list(&selected.sources)));
            }
        }
        let individual: Vec<_> = files
            .iter()
            .filter(|file| !covered.contains(&file.to_string()))
            .filter_map(|file| derived.existing(file))
            .collect();
        let connection = open_connection()?;
        if !individual.is_empty() {
            let sql = format!(
                "SELECT DISTINCT hex(left(station_id, 2)) FROM read_parquet([{}])",
                sql_string_list(&individual)
            );
            let mut statement = connection.prepare(&sql)?;
            let prefixes = statement
                .query_map([], |row| row.get::<_, Option<String>>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for prefix in prefixes {
                let predicate = prefix
                    .as_ref()
                    .map(|p| format!("hex(left(station_id, 2)) = '{}'", p.replace('\'', "''")))
                    .unwrap_or_else(|| "station_id IS NULL".to_owned());
                groups.entry(prefix.unwrap_or_else(|| "__NULL__".to_owned())).or_default().push(format!(
                    "SELECT {FORECAST_ROW_COLUMNS} FROM read_parquet([{}]) WHERE {overlap} AND {predicate}", sql_string_list(&individual)));
            }
        }
        if groups.is_empty() {
            return Ok(false);
        }
        let id = uuid::Uuid::now_v7();
        let temporary = self.root.join(format!("building-{id}.duckdb"));
        let scratch = self.root.join(format!("spill-{id}"));
        fs::create_dir_all(&scratch)?;
        let began = std::time::Instant::now();
        let result = (|| -> Result<bool, Error> {
            connection.execute_batch(&format!(
                "SET threads=1; SET preserve_insertion_order=false; SET temp_directory='{}'; SET max_temp_directory_size='4GB';
                 SET checkpoint_threshold='64MB'; ATTACH '{}' AS prepared (ROW_GROUP_SIZE 8192);",
                scratch.to_string_lossy().replace('\'', "''"), temporary.to_string_lossy().replace('\'', "''")))?;
            let first = groups.values().next().unwrap().first().unwrap();
            connection.execute_batch(&format!(
                "CREATE TABLE prepared.forecasts AS SELECT * FROM ({first}) WHERE false"
            ))?;
            let count = groups.len();
            for (index, rows) in groups.into_values().enumerate() {
                if stopping.is_cancelled() {
                    return Ok(false);
                }
                connection.execute_batch(&format!(
                    "INSERT INTO prepared.forecasts SELECT * FROM ({}) ORDER BY station_id, begin_ts, end_ts, generated_ts, published_ts, source",
                    rows.join(" UNION ALL ")))?;
                if fs::metadata(&temporary)?.len() > MAX_DATABASE_BYTES {
                    return Err(
                        io::Error::other("prepared forecast generation exceeds 1 GiB").into(),
                    );
                }
                if index % 32 == 31 {
                    info!(
                        "native forecasts {day}: {}/{count} station prefixes",
                        index + 1
                    );
                }
            }
            connection.execute_batch("CHECKPOINT prepared; DETACH prepared")?;
            if fs::metadata(&temporary)?.len() > MAX_DATABASE_BYTES {
                return Err(io::Error::other(
                    "prepared forecast generation exceeds 1 GiB after checkpoint",
                )
                .into());
            }
            Ok(true)
        })();
        drop(connection);
        let _ = fs::remove_dir_all(&scratch);
        if !matches!(result, Ok(true)) {
            let _ = fs::remove_file(&temporary);
            let _ = fs::remove_file(temporary.with_extension("duckdb.wal"));
            return result;
        }
        let file = format!("generation-{day}-{id}.duckdb");
        fs::rename(&temporary, self.root.join(&file))?;
        let sources = files
            .iter()
            .filter(|file| names.binary_search(&file.to_string()).is_ok())
            .map(|file| format!("{}/{file}", file.generated_at.date()))
            .collect();
        let manifest = Manifest {
            key,
            file,
            start,
            end,
            sources,
        };
        let manifest_tmp = self.root.join(format!("manifest-{id}.tmp"));
        fs::write(
            &manifest_tmp,
            serde_json::to_vec(&manifest).map_err(io::Error::other)?,
        )?;
        fs::rename(&manifest_tmp, self.manifest_path(day))?;
        info!(
            "native forecasts {day}: ready in {:.3}s, {} MiB",
            began.elapsed().as_secs_f64(),
            fs::metadata(self.root.join(&manifest.file))?.len() / 1024 / 1024
        );
        self.prune(retention_day)?;
        Ok(true)
    }

    /// Cross-process reader locks protect retired generations. Keep yesterday,
    /// today and any prewarmed tomorrow manifest; reclaim unreferenced files.
    pub fn cleanup(&self, today: Date) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("prepare.lock"))?;
        if lock.try_lock().is_ok() {
            self.prune(today)?;
        }
        Ok(())
    }

    fn prune(&self, today: Date) -> io::Result<()> {
        let mut keep = HashSet::new();
        let format = time::macros::format_description!("[year]-[month]-[day]");
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Some(day) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| Date::parse(s, format).ok())
            {
                if day < today.previous_day().unwrap_or(today) {
                    fs::remove_file(path)?;
                } else if let Some(manifest) = self.manifest(day) {
                    keep.insert(manifest.file);
                }
            }
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("generation-") && name.ends_with(".duckdb") && !keep.contains(&name)
            {
                let path = entry.path();
                let lock_path = path.with_extension("lock");
                let lock = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&lock_path)?;
                if lock.try_lock().is_ok() {
                    fs::remove_file(&path)?;
                    let _ = fs::remove_file(path.with_extension("bad"));
                    // UUID paths are never reused; future readers recheck the
                    // database after locking and fall back if it was removed.
                    let _ = fs::remove_file(lock_path);
                }
            }
        }
        Ok(())
    }
}
