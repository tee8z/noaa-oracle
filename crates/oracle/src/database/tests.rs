use super::*;
use crate::{events::EventStatus, signing::SigningKey};
use futures::poll;
use std::future::Future;
use time::Duration as TimeDuration;
use tokio::task::JoinHandle;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("database test timed out")
}

fn start(writer: DatabaseWriter) -> (CancellationToken, JoinHandle<Result<()>>) {
    let shutdown = CancellationToken::new();
    (shutdown.clone(), tokio::spawn(writer.run(shutdown)))
}

async fn open(capacity: usize) -> (tempfile::TempDir, Database, DatabaseWriter) {
    let directory = tempfile::tempdir().unwrap();
    let (database, writer) = bounded(Database::open_with_capacity(directory.path(), capacity))
        .await
        .unwrap();
    (directory, database, writer)
}

/// A stored-shape event; the announcement content does not matter here.
fn create_event_data(entries: usize) -> NewEvent {
    let directory = tempfile::tempdir().unwrap();
    let key = SigningKey::load_or_create(&directory.path().join("oracle.pem")).unwrap();
    let id = Uuid::now_v7();
    let start = (OffsetDateTime::now_utc() + TimeDuration::hours(1))
        .replace_nanosecond(0)
        .unwrap();
    NewEvent {
        id,
        source: "noaa_weather".into(),
        signing_date: start + TimeDuration::hours(3),
        start_observation_date: start,
        end_observation_date: start + TimeDuration::hours(1),
        locations: vec!["KORD".into(), "KSAW".into()],
        metrics: vec!["temp_high".into(), "wind_speed".into()],
        number_of_values_per_entry: 3,
        total_allowed_entries: entries.max(2),
        number_of_places_win: 1,
        nonce: key.new_event_nonce(id),
        event_announcement: EventLockingConditions {
            locking_points: vec![],
            expiry: Some(1),
        },
        coordinator_pubkey: "npub1coordinator".into(),
    }
}

async fn add(database: &Database, entries: usize) -> NewEvent {
    let event = create_event_data(entries);
    bounded(database.add_event(&event)).await.unwrap();
    event
}

fn entry(event_id: Uuid, station: &str) -> Entry {
    Entry {
        id: Uuid::now_v7(),
        event_id,
        picks: vec![
            Pick {
                target: station.into(),
                metric: "temp_high".into(),
                prediction: ValueOptions::Over,
            },
            Pick {
                target: station.into(),
                metric: "wind_speed".into(),
                prediction: ValueOptions::Par,
            },
        ],
        score: None,
        base_score: None,
    }
}

async fn event_count(database: &Database) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM events")
        .fetch_one(&database.readers)
        .await
        .unwrap()
}

/// Holds a real SQLite write lock until released, confined to the temporary
/// test database. The returned receiver fires once the lock is held.
async fn hold_write(
    database: &Database,
) -> (oneshot::Sender<()>, JoinHandle<Result<(), WriteError>>) {
    let (started, entered) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let database = database.clone();
    let caller = tokio::spawn(async move {
        database
            .write(move |connection| {
                Box::pin(async move {
                    let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
                    sqlx::query("INSERT INTO oracle_metadata (pubkey, name) VALUES (?, ?)")
                        .bind(vec![1u8; 32])
                        .bind("held")
                        .execute(&mut *transaction)
                        .await?;
                    let _ = started.send(());
                    let _ = released.await;
                    transaction.commit().await
                })
            })
            .await
    });
    bounded(entered).await.unwrap();
    (release, caller)
}

#[tokio::test]
async fn zero_capacity_is_rejected_before_creating_a_database() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        Database::open_with_capacity(directory.path(), 0)
            .await
            .is_err()
    );
    assert!(!directory.path().join(DATABASE_FILE).exists());
}

#[tokio::test]
async fn writes_reply_only_after_commit_and_clones_share_the_writer() {
    let (_directory, database, writer) = open(128).await;
    let (shutdown, task) = start(writer);
    let (release, holder) = hold_write(&database).await;
    let event = create_event_data(2);
    let event_id = event.id;
    let clone = database.clone();
    let inserted = event.clone();
    let mut pending = Box::pin(async move { clone.add_event(&inserted).await });
    assert!(poll!(pending.as_mut()).is_pending());
    assert_eq!(event_count(&database).await, 0, "reply preceded commit");
    assert_eq!(
        database.commands.capacity(),
        127,
        "clones must share one queue"
    );

    release.send(()).unwrap();
    bounded(holder).await.unwrap().unwrap();
    bounded(pending).await.unwrap();
    assert_eq!(event_count(&database).await, 1);
    let stored = bounded(database.get_event(event_id))
        .await
        .unwrap()
        .expect("event exists");
    assert_eq!(stored.nonce, event.nonce);
    assert_eq!(stored.locations, event.locations);
    assert_eq!(stored.metrics, event.metrics);
    assert_eq!(stored.signing_date, event.signing_date);
    assert_eq!(
        stored.status(event.start_observation_date),
        EventStatus::Running
    );
    assert_eq!(stored.total_entries, 0);
    assert_eq!(
        database.event_announcement(event_id).await.unwrap(),
        Some(event.event_announcement.clone())
    );
    assert!(database.is_ready().await);

    shutdown.cancel();
    bounded(task).await.unwrap().unwrap();
    assert!(!database.is_writer_available());
}

#[tokio::test]
async fn full_queue_rejects_before_admission_and_shutdown_drains_dropped_replies() {
    let (directory, database, writer) = open(2).await;
    let migrations: Vec<(i64, bool)> =
        sqlx::query_as("SELECT version, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&database.readers)
            .await
            .unwrap();
    assert!(!migrations.is_empty());
    let (shutdown, task) = start(writer);
    let (release, holder) = hold_write(&database).await;
    let first = create_event_data(2);
    let first_id = first.id;
    let mut dropped = Box::pin(database.add_event(&first));
    assert!(poll!(dropped.as_mut()).is_pending());
    let second = create_event_data(2);
    let second_id = second.id;
    let mut retained = Box::pin(database.add_event(&second));
    assert!(poll!(retained.as_mut()).is_pending());
    assert_eq!(database.commands.capacity(), 0);
    assert!(matches!(
        database.add_event(&create_event_data(2)).await,
        Err(WriteError::Unavailable)
    ));
    // The caller of the first admitted write goes away; the write still runs.
    drop(dropped);

    shutdown.cancel();
    release.send(()).unwrap();
    bounded(holder).await.unwrap().unwrap();
    bounded(retained).await.unwrap();
    bounded(task).await.unwrap().unwrap();
    assert!(!database.is_ready().await);
    assert!(matches!(
        database.add_event(&create_event_data(2)).await,
        Err(WriteError::Unavailable)
    ));

    // Reopening preserves admitted commits and applied migrations.
    let (reopened, writer) = bounded(Database::open(directory.path())).await.unwrap();
    assert_eq!(event_count(&reopened).await, 2);
    assert!(reopened.get_event(first_id).await.unwrap().is_some());
    assert!(reopened.get_event(second_id).await.unwrap().is_some());
    let reopened_migrations: Vec<(i64, bool)> =
        sqlx::query_as("SELECT version, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&reopened.readers)
            .await
            .unwrap();
    assert_eq!(reopened_migrations, migrations);
    let (shutdown, task) = start(writer);
    shutdown.cancel();
    bounded(task).await.unwrap().unwrap();
}

#[tokio::test]
async fn waiting_writes_queue_behind_capacity_instead_of_failing() {
    let (_directory, database, writer) = open(1).await;
    let (shutdown, task) = start(writer);
    let event = add(&database, 2).await;
    let (release, holder) = hold_write(&database).await;
    let queued_event = create_event_data(2);
    let mut queued = Box::pin(database.add_event(&queued_event));
    assert!(poll!(queued.as_mut()).is_pending());
    assert_eq!(database.commands.capacity(), 0);
    let mut waiting = Box::pin(database.record_attestation(event.id, MaybeScalar::Zero));
    assert!(poll!(waiting.as_mut()).is_pending());

    release.send(()).unwrap();
    bounded(holder).await.unwrap().unwrap();
    bounded(queued).await.unwrap();
    assert!(bounded(waiting).await.unwrap());
    let signed = database.get_event(event.id).await.unwrap().unwrap();
    assert_eq!(signed.attestation, Some(MaybeScalar::Zero));
    let unattested = database.unattested_events().await.unwrap();
    assert!(unattested.iter().all(|active| active.id != event.id));
    assert_eq!(unattested.len(), 1, "the queued event is still unattested");
    shutdown.cancel();
    bounded(task).await.unwrap().unwrap();
}

#[tokio::test]
async fn an_attestation_is_never_replaced() {
    let (_directory, database, writer) = open(8).await;
    let (shutdown, task) = start(writer);
    let event = add(&database, 2).await;
    let first = MaybeScalar::from_slice(&[7; 32]).unwrap();
    assert!(
        bounded(database.record_attestation(event.id, first))
            .await
            .unwrap()
    );
    assert!(
        !bounded(database.record_attestation(event.id, MaybeScalar::Zero))
            .await
            .unwrap()
    );
    let stored = database.get_event(event.id).await.unwrap().unwrap();
    assert_eq!(stored.attestation, Some(first));
    shutdown.cancel();
    bounded(task).await.unwrap().unwrap();
}

#[tokio::test]
async fn entries_readings_and_scores_round_trip() {
    let (_directory, database, writer) = open(8).await;
    let (shutdown, task) = start(writer);
    let event = add(&database, 2).await;
    let first = entry(event.id, "KORD");
    let second = entry(event.id, "KSAW");
    let (first, second) = if first.id < second.id {
        (first, second)
    } else {
        (second, first)
    };
    assert!(
        bounded(database.add_event_entries(event.id, vec![first.clone(), second.clone()]))
            .await
            .unwrap()
    );
    assert!(
        !bounded(database.add_event_entries(event.id, vec![entry(event.id, "KORD")]))
            .await
            .unwrap(),
        "a second submission writes nothing"
    );

    let entries = database.event_entries(event.id).await.unwrap();
    assert_eq!(entries, vec![first.clone(), second.clone()]);
    assert_eq!(
        database.event_entry(event.id, second.id).await.unwrap(),
        Some(second.clone())
    );
    assert!(
        database
            .event_entry(event.id, Uuid::now_v7())
            .await
            .unwrap()
            .is_none()
    );

    let reading = |target: &str, metric: &str, observed| Reading {
        target: target.into(),
        metric: metric.into(),
        baseline: Some(60.0),
        observed,
    };
    let readings = vec![
        reading("KORD", "temp_high", Some(61.5)),
        reading("KSAW", "temp_high", None),
    ];
    bounded(database.replace_readings(event.id, readings.clone()))
        .await
        .unwrap();
    bounded(database.replace_readings(event.id, readings.clone()))
        .await
        .unwrap();
    assert_eq!(
        database
            .readings(&[event.id])
            .await
            .unwrap()
            .remove(&event.id),
        Some(readings),
        "replacing readings does not accumulate rows"
    );

    bounded(database.update_entry_scores(vec![
        EntryScore {
            id: first.id,
            total_score: 20_000,
            base_score: 20,
        },
        EntryScore {
            id: second.id,
            total_score: 10_000,
            base_score: 0,
        },
    ]))
    .await
    .unwrap();
    let scored = database.event_entries(event.id).await.unwrap();
    assert_eq!(scored[0].score, Some(20_000));
    assert_eq!(scored[0].base_score, Some(20));
    assert_eq!(
        scored[1].base_score,
        Some(0),
        "a zero score is still a score"
    );

    let listed = database.list_events(&[event.id], 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].total_entries, 2);
    assert_eq!(database.list_events(&[], 10).await.unwrap().len(), 1);
    shutdown.cancel();
    bounded(task).await.unwrap().unwrap();
}

#[tokio::test]
async fn readers_are_read_only_and_query_only() {
    let (_directory, database, writer) = open(8).await;
    let (shutdown, task) = start(writer);
    let mut reader = database.readers.acquire().await.unwrap();
    let query_only: i64 = sqlx::query_scalar("PRAGMA query_only")
        .fetch_one(&mut *reader)
        .await
        .unwrap();
    assert_eq!(query_only, 1);
    assert!(
        sqlx::query("INSERT INTO oracle_metadata (pubkey, name) VALUES (X'00', 'reader')")
            .execute(&mut *reader)
            .await
            .is_err()
    );
    sqlx::query("PRAGMA query_only=OFF")
        .execute(&mut *reader)
        .await
        .unwrap();
    assert!(
        sqlx::query("INSERT INTO oracle_metadata (pubkey, name) VALUES (X'00', 'reader')")
            .execute(&mut *reader)
            .await
            .is_err(),
        "read-only connections reject writes even without query_only"
    );
    drop(reader);
    shutdown.cancel();
    bounded(task).await.unwrap().unwrap();
}

#[tokio::test]
async fn oracle_metadata_is_stored_once() {
    let (_directory, database, writer) = open(8).await;
    let (shutdown, task) = start(writer);
    assert!(database.get_stored_public_key().await.unwrap().is_none());
    let key = dlctix::musig2::secp256k1::SecretKey::new(&mut rand::rng());
    let pubkey = key
        .x_only_public_key(&dlctix::musig2::secp256k1::Secp256k1::new())
        .0;
    bounded(database.add_oracle_metadata(pubkey)).await.unwrap();
    bounded(database.add_oracle_metadata(pubkey)).await.unwrap();
    assert_eq!(
        database.get_stored_public_key().await.unwrap(),
        Some(pubkey)
    );
    shutdown.cancel();
    bounded(task).await.unwrap().unwrap();
}
