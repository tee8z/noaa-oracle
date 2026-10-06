//! The Nostr outbox (see [`crate::publication`]). Rows are queued after
//! the event write they follow; the publisher runs in the background, so
//! its writes wait for queue capacity.

use sqlx::{AssertSqlSafe, Connection, Row};
use uuid::Uuid;

use super::{Database, WriteError, decode_error};
use crate::publication::Stage;

/// Wait after the first failed attempt; doubled after each further one.
const FIRST_RETRY_SECONDS: i64 = 30;
/// Longest wait between attempts.
const MAX_RETRY_SECONDS: i64 = 3600;
/// Relay errors are kept short; they are only read by operators.
const MAX_ERROR_CHARS: usize = 500;

/// An outbox row that is due.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuePublication {
    pub event_id: Uuid,
    pub stage: Stage,
    pub relay: String,
}

impl Database {
    /// Queues `stage` of `event_id` for each relay. Rows already queued or
    /// published are kept as they are.
    pub async fn queue_publication(
        &self,
        event_id: Uuid,
        stage: Stage,
        relays: Vec<String>,
        now: i64,
    ) -> Result<(), WriteError> {
        self.write(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                for relay in relays {
                    sqlx::query(
                        "INSERT OR IGNORE INTO nostr_outbox
                            (event_id, stage, relay, queued_at, next_attempt_at)
                         VALUES (?1, ?2, ?3, ?4, ?4)",
                    )
                    .bind(event_id.to_string())
                    .bind(stage.as_str())
                    .bind(relay)
                    .bind(now)
                    .execute(&mut *transaction)
                    .await?;
                }
                transaction.commit().await
            })
        })
        .await
    }

    /// Queues every event whose signing date is at or after `since` for
    /// each relay: its announcement, and its attestation once signed.
    /// Returns the rows added; rows already queued or published are kept.
    pub async fn queue_recent_publications(
        &self,
        since: i64,
        relays: Vec<String>,
        now: i64,
    ) -> Result<u64, WriteError> {
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                let mut added = 0;
                for relay in relays {
                    added += sqlx::query(
                        "INSERT OR IGNORE INTO nostr_outbox
                            (event_id, stage, relay, queued_at, next_attempt_at)
                         SELECT id, 'announced', ?1, ?2, ?2 FROM events
                         WHERE signing_date >= ?3
                         UNION ALL
                         SELECT id, 'attested', ?1, ?2, ?2 FROM events
                         WHERE signing_date >= ?3 AND attestation IS NOT NULL",
                    )
                    .bind(&relay)
                    .bind(now)
                    .bind(since)
                    .execute(&mut *transaction)
                    .await?
                    .rows_affected();
                }
                transaction.commit().await?;
                Ok(added)
            })
        })
        .await
    }

    /// Unpublished rows for `relays` due at `now`, longest waiting first.
    pub async fn due_publications(
        &self,
        relays: &[String],
        now: i64,
        limit: usize,
    ) -> Result<Vec<DuePublication>, sqlx::Error> {
        if relays.is_empty() {
            return Ok(vec![]);
        }
        // Only `?` placeholders are interpolated; relays are bound.
        let sql = format!(
            "SELECT event_id, stage, relay FROM nostr_outbox
             WHERE published_at IS NULL AND next_attempt_at <= ? AND relay IN ({})
             ORDER BY next_attempt_at, event_id, relay LIMIT ?",
            vec!["?"; relays.len()].join(",")
        );
        let mut query = sqlx::query(AssertSqlSafe(sql)).bind(now);
        for relay in relays {
            query = query.bind(relay);
        }
        let rows = query
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&self.readers)
            .await?;
        rows.iter()
            .map(|row| {
                let event_id: String = row.try_get("event_id")?;
                let stage: String = row.try_get("stage")?;
                Ok(DuePublication {
                    event_id: Uuid::parse_str(&event_id)
                        .map_err(|error| decode_error("event_id", error))?,
                    stage: Stage::parse(&stage)
                        .ok_or_else(|| decode_error("stage", format!("unknown stage {stage}")))?,
                    relay: row.try_get("relay")?,
                })
            })
            .collect()
    }

    /// Rows for `relays` not yet published, due or waiting to retry.
    pub async fn publication_backlog(&self, relays: &[String]) -> Result<u64, sqlx::Error> {
        if relays.is_empty() {
            return Ok(0);
        }
        // Only `?` placeholders are interpolated; relays are bound.
        let sql = format!(
            "SELECT COUNT(*) FROM nostr_outbox
             WHERE published_at IS NULL AND relay IN ({})",
            vec!["?"; relays.len()].join(",")
        );
        let mut query = sqlx::query_scalar(AssertSqlSafe(sql));
        for relay in relays {
            query = query.bind(relay);
        }
        let count: i64 = query.fetch_one(&self.readers).await?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// The newest `created_at` among Nostr events published for `event_id`.
    pub async fn latest_publication(&self, event_id: Uuid) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar("SELECT MAX(nostr_created_at) FROM nostr_outbox WHERE event_id = ?")
            .bind(event_id.to_string())
            .fetch_one(&self.readers)
            .await
    }

    /// Marks `stages` of `event_id` published to `relay` as the Nostr
    /// event `nostr_event_id`.
    pub async fn publication_sent(
        &self,
        event_id: Uuid,
        relay: String,
        stages: Vec<Stage>,
        nostr_event_id: String,
        created_at: i64,
        now: i64,
    ) -> Result<(), WriteError> {
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                for stage in stages {
                    sqlx::query(
                        "UPDATE nostr_outbox
                         SET published_at = ?, nostr_event_id = ?, nostr_created_at = ?,
                             attempts = attempts + 1, last_error = NULL
                         WHERE event_id = ? AND stage = ? AND relay = ? AND published_at IS NULL",
                    )
                    .bind(now)
                    .bind(&nostr_event_id)
                    .bind(created_at)
                    .bind(event_id.to_string())
                    .bind(stage.as_str())
                    .bind(&relay)
                    .execute(&mut *transaction)
                    .await?;
                }
                transaction.commit().await
            })
        })
        .await
    }

    /// Records a failed attempt for `stages` of `event_id` on `relay` and
    /// schedules the next: 30 seconds after the first failure, doubling up
    /// to an hour.
    pub async fn publication_failed(
        &self,
        event_id: Uuid,
        relay: String,
        stages: Vec<Stage>,
        error: String,
        now: i64,
    ) -> Result<(), WriteError> {
        let error: String = error.chars().take(MAX_ERROR_CHARS).collect();
        self.write_waiting(move |connection| {
            Box::pin(async move {
                let mut transaction = connection.begin().await?;
                for stage in stages {
                    sqlx::query(
                        "UPDATE nostr_outbox
                         SET next_attempt_at = ?1 + MIN(?2, ?3 << MIN(attempts, 10)),
                             attempts = attempts + 1, last_error = ?4
                         WHERE event_id = ?5 AND stage = ?6 AND relay = ?7
                           AND published_at IS NULL",
                    )
                    .bind(now)
                    .bind(MAX_RETRY_SECONDS)
                    .bind(FIRST_RETRY_SECONDS)
                    .bind(&error)
                    .bind(event_id.to_string())
                    .bind(stage.as_str())
                    .bind(&relay)
                    .execute(&mut *transaction)
                    .await?;
                }
                transaction.commit().await
            })
        })
        .await
    }

    /// Drops the unpublished rows of an event that no longer exists.
    pub async fn drop_publications(&self, event_id: Uuid) -> Result<(), WriteError> {
        self.write_waiting(move |connection| {
            Box::pin(async move {
                sqlx::query("DELETE FROM nostr_outbox WHERE event_id = ? AND published_at IS NULL")
                    .bind(event_id.to_string())
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .await
    }
}
