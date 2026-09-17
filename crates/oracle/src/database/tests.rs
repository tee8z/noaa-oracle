use super::*;
use crate::events::{CreateEvent, EventStatus, ScoringField};
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

fn create_event_data(entries: usize) -> CreateEventData {
    let start = OffsetDateTime::now_utc() + TimeDuration::hours(1);
    let event = CreateEvent {
        id: Uuid::now_v7(),
        signing_date: start + TimeDuration::hours(3),
        start_observation_date: start,
        end_observation_date: start + TimeDuration::hours(1),
        locations: vec!["KORD".into(), "KSAW".into()],
        number_of_values_per_entry: 3,
        total_allowed_entries: entries,
        number_of_places_win: 1,
        scoring_fields: ScoringField::defaults(),
    };
    let oracle = Scalar::random(&mut rand::rng()).base_point_mul();
    CreateEventData::new(oracle, nostr::key::Keys::generate().public_key(), event).unwrap()
}

fn entry(event_id: Uuid, station: &str) -> WeatherEntry {
    WeatherEntry {
        id: Uuid::now_v7(),
        event_id,
        expected_observations: vec![WeatherChoices {
            stations: station.into(),
            temp_high: Some(ValueOptions::Over),
            temp_low: None,
            wind_speed: Some(ValueOptions::Par),
            wind_direction: None,
            rain_amt: None,
            snow_amt: None,
            humidity: None,
        }],
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
    let mut pending = Box::pin(async move { clone.add_event(event).await });
    assert!(poll!(pending.as_mut()).is_pending());
    assert_eq!(event_count(&database).await, 0, "reply preceded commit");
    assert_eq!(
        database.commands.capacity(),
        127,
        "clones must share one queue"
    );

    release.send(()).unwrap();
    bounded(holder).await.unwrap().unwrap();
    let created = bounded(pending).await.unwrap();
    assert_eq!(created.id, event_id);
    assert_eq!(created.status, EventStatus::Live);
    assert_eq!(event_count(&database).await, 1);
    let stored = bounded(database.get_event(&event_id))
        .await
        .unwrap()
        .expect("event exists");
    assert_eq!(stored.nonce, created.nonce);
    assert_eq!(stored.event_announcement, created.event_announcement);
    assert_eq!(stored.locations, created.locations);
    assert!(stored.entries.is_empty());
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
    let first = create_event_data(1);
    let first_id = first.id;
    let mut dropped = Box::pin(database.add_event(first));
    assert!(poll!(dropped.as_mut()).is_pending());
    let second = create_event_data(1);
    let second_id = second.id;
    let mut retained = Box::pin(database.add_event(second));
    assert!(poll!(retained.as_mut()).is_pending());
    assert_eq!(database.commands.capacity(), 0);
    assert!(matches!(
        database.add_event(create_event_data(1)).await,
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
        database.add_event(create_event_data(1)).await,
        Err(WriteError::Unavailable)
    ));

    // Reopening preserves admitted commits and applied migrations.
    let (reopened, writer) = bounded(Database::open(directory.path())).await.unwrap();
    assert_eq!(event_count(&reopened).await, 2);
    assert!(reopened.get_event(&first_id).await.unwrap().is_some());
    assert!(reopened.get_event(&second_id).await.unwrap().is_some());
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
    let event = bounded(database.add_event(create_event_data(1)))
        .await
        .unwrap();
    let (release, holder) = hold_write(&database).await;
    let mut queued = Box::pin(database.add_event(create_event_data(1)));
    assert!(poll!(queued.as_mut()).is_pending());
    assert_eq!(database.commands.capacity(), 0);
    let mut waiting = Box::pin(database.update_event_attestation(event.id, MaybeScalar::Zero));
    assert!(poll!(waiting.as_mut()).is_pending());

    release.send(()).unwrap();
    bounded(holder).await.unwrap().unwrap();
    bounded(queued).await.unwrap();
    bounded(waiting).await.unwrap();
    let signed = database.get_event(&event.id).await.unwrap().unwrap();
    assert_eq!(signed.attestation, Some(MaybeScalar::Zero));
    assert_eq!(signed.status, EventStatus::Signed);
    let active = database.get_active_events().await.unwrap();
    assert!(active.iter().all(|active| active.id != event.id));
    assert_eq!(active.len(), 1, "the queued event is still active");
    shutdown.cancel();
    bounded(task).await.unwrap().unwrap();
}

#[tokio::test]
async fn entries_weather_and_scores_round_trip() {
    let (_directory, database, writer) = open(8).await;
    let (shutdown, task) = start(writer);
    let event = bounded(database.add_event(create_event_data(2)))
        .await
        .unwrap();
    let first = entry(event.id, "KORD");
    let second = entry(event.id, "KSAW");
    bounded(database.add_event_entries(vec![first.clone(), second.clone()]))
        .await
        .unwrap();

    let loaded = database.get_event(&event.id).await.unwrap().unwrap();
    assert_eq!(loaded.entries.len(), 2);
    assert_eq!(loaded.entry_ids, vec![first.id, second.id]);
    let stored_first = loaded
        .entries
        .iter()
        .find(|entry| entry.id == first.id)
        .unwrap();
    assert_eq!(
        stored_first.expected_observations,
        first.expected_observations
    );
    assert_eq!(stored_first.score, None);
    assert_eq!(
        database
            .get_weather_entry(&event.id, &second.id)
            .await
            .unwrap()
            .unwrap()
            .expected_observations,
        second.expected_observations
    );
    assert!(
        database
            .get_weather_entry(&event.id, &Uuid::now_v7())
            .await
            .unwrap()
            .is_none()
    );

    let observed = Observed {
        date: OffsetDateTime::now_utc().replace_nanosecond(0).unwrap(),
        temp_low: 40,
        temp_high: 60,
        wind_speed: 5,
    };
    let weather = vec![
        Weather {
            station_id: "KORD".into(),
            observed: Some(observed.clone()),
            forecasted: Forecasted {
                date: observed.date,
                temp_low: 41,
                temp_high: 61,
                wind_speed: None,
            },
        },
        Weather {
            station_id: "KSAW".into(),
            observed: None,
            forecasted: Forecasted {
                date: observed.date,
                temp_low: 30,
                temp_high: 50,
                wind_speed: Some(12),
            },
        },
    ];
    bounded(database.update_weather_station_data(event.id, weather.clone()))
        .await
        .unwrap();
    assert_eq!(database.get_event_weather(event.id).await.unwrap(), weather);

    bounded(database.update_entry_scores(vec![(first.id, 20_000, 20), (second.id, 10_000, 10)]))
        .await
        .unwrap();
    let scored = database.get_event_weather_entries(&event.id).await.unwrap();
    let first_score = scored.iter().find(|entry| entry.id == first.id).unwrap();
    assert_eq!(first_score.score, Some(20_000));
    assert_eq!(first_score.base_score, Some(20));

    let active = database.get_active_events().await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].total_entries, 2);
    let summaries = database
        .filtered_list_events(EventFilter {
            limit: Some(10),
            event_ids: Some(vec![event.id]),
        })
        .await
        .unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].total_entries, 2);
    assert_eq!(summaries[0].weather, weather);
    let to_sign = database.get_events_to_sign(&[event.id]).await.unwrap();
    assert_eq!(to_sign.len(), 1);
    assert_eq!(to_sign[0].nonce, event.nonce);
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
