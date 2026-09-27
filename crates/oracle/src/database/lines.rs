//! Line history and fits (see [`crate::lines`]). Writes come from the line
//! pass, a background producer, so they wait for queue capacity.

use std::collections::HashMap;

use sqlx::{Connection, Row};
use time::OffsetDateTime;

use super::{Database, WriteError, line_from_row, timestamp};
use crate::lines::{Line, LinePair};

impl Database {
    /// Windows of `hours` starting at or after `since` that the line pass
    /// has read: pairs stored and when it read them, by window start.
    pub async fn line_windows(
        &self,
        source: &str,
        hours: i64,
        since: OffsetDateTime,
    ) -> Result<HashMap<OffsetDateTime, (i64, OffsetDateTime)>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT window_start, pairs, collected_at FROM line_windows
             WHERE source = ? AND window_hours = ? AND window_start >= ?",
        )
        .bind(source)
        .bind(hours)
        .bind(since.unix_timestamp())
        .fetch_all(&self.readers)
        .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    timestamp(row.try_get("window_start")?, "window_start")?,
                    (
                        row.try_get("pairs")?,
                        timestamp(row.try_get("collected_at")?, "collected_at")?,
                    ),
                ))
            })
            .collect()
    }

    /// Stores one window's pairs, replacing any from an earlier read, and
    /// records the read.
    pub async fn store_line_window(
        &self,
        source: &str,
        hours: i64,
        start: OffsetDateTime,
        pairs: Vec<LinePair>,
        read_at: OffsetDateTime,
    ) -> Result<(), WriteError> {
        let source = source.to_owned();
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                sqlx::query(
                    "DELETE FROM line_pairs
                     WHERE source = ? AND window_hours = ? AND window_start = ?",
                )
                .bind(&source)
                .bind(hours)
                .bind(start.unix_timestamp())
                .execute(&mut *transaction)
                .await?;
                for pair in &pairs {
                    sqlx::query(
                        "INSERT OR REPLACE INTO line_pairs
                         (source, window_hours, metric, target, window_start, baseline, observed)
                         VALUES (?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(&source)
                    .bind(hours)
                    .bind(&pair.metric)
                    .bind(&pair.target)
                    .bind(pair.window_start.unix_timestamp())
                    .bind(pair.baseline)
                    .bind(pair.observed)
                    .execute(&mut *transaction)
                    .await?;
                }
                sqlx::query(
                    "INSERT INTO line_windows (source, window_hours, window_start, pairs, collected_at)
                     VALUES (?, ?, ?, ?, ?)
                     ON CONFLICT (source, window_hours, window_start) DO UPDATE
                     SET pairs = excluded.pairs, collected_at = excluded.collected_at",
                )
                .bind(&source)
                .bind(hours)
                .bind(start.unix_timestamp())
                .bind(i64::try_from(pairs.len()).unwrap_or(i64::MAX))
                .bind(read_at.unix_timestamp())
                .execute(&mut *transaction)
                .await?;
                transaction.commit().await
            })
        })
        .await
    }

    /// Pairs of one metric from `hours` windows starting at or after
    /// `since`. One metric at a time keeps a refit's memory to a quarter of
    /// the history.
    pub async fn line_pairs(
        &self,
        source: &str,
        hours: i64,
        metric: &str,
        since: OffsetDateTime,
    ) -> Result<Vec<LinePair>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT metric, target, window_start, baseline, observed FROM line_pairs
             WHERE source = ? AND window_hours = ? AND metric = ? AND window_start >= ?
             ORDER BY target, window_start",
        )
        .bind(source)
        .bind(hours)
        .bind(metric)
        .bind(since.unix_timestamp())
        .fetch_all(&self.readers)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(LinePair {
                    target: row.try_get("target")?,
                    metric: row.try_get("metric")?,
                    window_start: timestamp(row.try_get("window_start")?, "window_start")?,
                    baseline: row.try_get("baseline")?,
                    observed: row.try_get("observed")?,
                })
            })
            .collect()
    }

    /// When the stored fit for `hours` windows was made, if there is one.
    pub async fn line_fitted_at(
        &self,
        source: &str,
        hours: i64,
    ) -> Result<Option<OffsetDateTime>, sqlx::Error> {
        let fitted: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(fitted_at) FROM line_fits WHERE source = ? AND window_hours = ?",
        )
        .bind(source)
        .bind(hours)
        .fetch_one(&self.readers)
        .await?;
        fitted
            .map(|seconds| timestamp(seconds, "fitted_at"))
            .transpose()
    }

    /// Replaces every stored line for `hours` windows with `lines`.
    pub async fn replace_line_fits(
        &self,
        source: &str,
        hours: i64,
        lines: Vec<Line>,
    ) -> Result<(), WriteError> {
        let source = source.to_owned();
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                sqlx::query("DELETE FROM line_fits WHERE source = ? AND window_hours = ?")
                    .bind(&source)
                    .bind(hours)
                    .execute(&mut *transaction)
                    .await?;
                for line in &lines {
                    sqlx::query(
                        "INSERT INTO line_fits (
                            source, window_hours, metric, target, lower, upper, windows,
                            over, par, under, first_window, last_window, fitted_at
                        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(&source)
                    .bind(hours)
                    .bind(&line.metric)
                    .bind(&line.target)
                    .bind(line.lower)
                    .bind(line.upper)
                    .bind(line.windows)
                    .bind(line.over)
                    .bind(line.par)
                    .bind(line.under)
                    .bind(line.first_window.unix_timestamp())
                    .bind(line.last_window.unix_timestamp())
                    .bind(line.fitted_at.unix_timestamp())
                    .execute(&mut *transaction)
                    .await?;
                }
                transaction.commit().await
            })
        })
        .await
    }

    /// Stored lines of `hours` windows for `targets` and `metrics`, with the
    /// pooled line (empty target) of each metric.
    pub async fn line_fits(
        &self,
        source: &str,
        hours: i64,
        targets: &[String],
        metrics: &[String],
    ) -> Result<Vec<Line>, sqlx::Error> {
        let json = |values: &[String]| serde_json::to_string(values).expect("strings serialize");
        let rows = sqlx::query(
            "SELECT target, metric, lower, upper,
                    CASE target WHEN '' THEN 'pooled' ELSE 'station' END AS level,
                    window_hours, windows, over, par, under, first_window, last_window, fitted_at
             FROM line_fits
             WHERE source = ? AND window_hours = ?
               AND metric IN (SELECT value FROM json_each(?))
               AND (target = '' OR target IN (SELECT value FROM json_each(?)))
             ORDER BY metric, target",
        )
        .bind(source)
        .bind(hours)
        .bind(json(metrics))
        .bind(json(targets))
        .fetch_all(&self.readers)
        .await?;
        rows.iter().map(line_from_row).collect()
    }

    /// Drops history of windows that started before `before`.
    pub async fn prune_line_history(
        &self,
        source: &str,
        hours: i64,
        before: OffsetDateTime,
    ) -> Result<(), WriteError> {
        let source = source.to_owned();
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                for table in ["line_pairs", "line_windows"] {
                    // Only the two constant table names are interpolated.
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "DELETE FROM {table}
                         WHERE source = ? AND window_hours = ? AND window_start < ?"
                    )))
                    .bind(&source)
                    .bind(hours)
                    .bind(before.unix_timestamp())
                    .execute(&mut *transaction)
                    .await?;
                }
                transaction.commit().await
            })
        })
        .await
    }
}
