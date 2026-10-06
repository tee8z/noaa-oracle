use super::*;
use crate::{
    scoring::ScoringRules,
    signing::SigningKey,
    statement::{
        ObservationTerms, Outcomes as StatementOutcomes, RankingOutcomes, Statement, Terms,
    },
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Mutex,
        atomic::{AtomicI64, Ordering},
    },
};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

const SIGNING_DATE: i64 = 1_790_000_000;
const URL: &str = "https://oracle.example.com/oracle/events/x";

struct Fixture {
    _directory: tempfile::TempDir,
    oracle: SigningKey,
    key: PublishingKey,
    event_id: Uuid,
    nonce: crate::signing::EventNonce,
    locking_points: Vec<MaybePoint>,
}

/// An event with three entries and one winner: outcomes `[0]`, `[1]`,
/// `[2]`, then the refund-all outcome.
fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let oracle = SigningKey::load_or_create(&directory.path().join("oracle.pem")).unwrap();
    let key = PublishingKey::load_or_create(&directory.path().join("nostr.pem")).unwrap();
    let event_id = Uuid::now_v7();
    let nonce = oracle.new_event_nonce(event_id);
    let locking_points = scoring::ranking_outcomes(3, 1)
        .iter()
        .map(|winners| oracle.locking_point(nonce.point, &scoring::outcome_message(winners)))
        .collect();
    Fixture {
        _directory: directory,
        oracle,
        key,
        event_id,
        nonce,
        locking_points,
    }
}

impl Fixture {
    fn announcement(&self) -> Record {
        Record::announcement(
            self.event_id,
            Point::from(self.oracle.public_key()),
            self.nonce.point,
            SIGNING_DATE,
            Some(1_790_086_400),
            Outcomes::Ranking {
                entries: 3,
                places: 1,
            },
            &self.locking_points,
        )
    }

    fn attest(&self, winners: &[usize]) -> MaybeScalar {
        self.oracle
            .attest(
                self.event_id,
                &self.nonce,
                &self.locking_points,
                &scoring::outcome_message(winners),
            )
            .unwrap()
    }

    fn statement(&self, nonce_point: Point) -> SignedStatement {
        self.oracle.sign_statement(Statement {
            event_id: self.event_id,
            signing_date: SIGNING_DATE,
            expiry: 1_790_086_400,
            nonce_point,
            outcomes: StatementOutcomes::Ranking(RankingOutcomes {
                number_of_places_win: 1,
                entry_ids: (1..=3).map(Uuid::from_u128).collect(),
            }),
            terms: Terms::Observation(ObservationTerms {
                source: "noaa_weather".into(),
                start_observation_date: SIGNING_DATE - 86_400,
                end_observation_date: SIGNING_DATE,
                targets: vec!["KORD".into()],
                scoring_fields: vec!["temp_high".into()],
                number_of_values_per_entry: 1,
                scoring_rules: ScoringRules::Fixed,
                lines: vec![],
            }),
        })
    }
}

#[test]
fn attestations_round_trip_through_the_published_event() {
    let fixture = fixture();
    let attestation = fixture.attest(&[2]);
    let record = fixture
        .announcement()
        .attested(attestation, &fixture.locking_points);
    assert_eq!(record.outcome_index, Some(2));
    assert_eq!(record.winners, Some(vec![2]));

    let event = build_event(
        &fixture.key,
        &record,
        URL,
        Timestamp::from_secs(1_790_000_100),
    )
    .unwrap();
    event.verify().unwrap();
    assert_eq!(event.kind, Kind::Custom(30078));
    assert_eq!(event.pubkey, fixture.key.public_key());
    assert_ne!(
        event.pubkey.to_bytes(),
        fixture.oracle.x_only_public_key().serialize(),
        "events are never signed by the attestation key"
    );
    assert_eq!(
        event.tags.identifier(),
        Some(format!("oracle:{}", fixture.event_id))
    );
    let tag = |name: &str| {
        event
            .tags
            .iter()
            .find(|tag| tag.kind() == name)
            .and_then(|tag| tag.content())
            .map(str::to_owned)
    };
    assert_eq!(tag("stage").as_deref(), Some("attested"));
    assert_eq!(tag("r").as_deref(), Some(URL));
    let oracle_hex = Point::from(fixture.oracle.public_key())
        .serialize()
        .to_lower_hex_string();
    assert_eq!(tag("oracle"), Some(oracle_hex.clone()));

    // What a reader without this crate sees.
    let content: serde_json::Value = serde_json::from_str(&event.content).unwrap();
    assert_eq!(content["v"], 1);
    assert_eq!(content["type"], "oracle-event");
    assert_eq!(content["event_id"], fixture.event_id.to_string());
    assert_eq!(content["oracle_pubkey"], oracle_hex);
    assert_eq!(
        content["outcomes"],
        serde_json::json!({"kind": "ranking", "entries": 3, "places": 1})
    );
    assert_eq!(
        content["attestation"],
        attestation.serialize().to_lower_hex_string()
    );
    assert_eq!(content["outcome_index"], 2);

    let parsed = parse_event(&event).unwrap();
    assert_eq!(parsed, record);
    assert_eq!(parsed.verify(), Ok(Some(2)));
    // The check the oracle makes when it attests: `s·G` is the outcome's
    // locking point.
    assert_eq!(
        parsed.attestation.unwrap().base_point_mul(),
        attestation_locking_point(
            fixture.oracle.public_key(),
            fixture.nonce.point,
            scoring::outcome_message(&[2])
        )
    );
    assert_eq!(
        parsed.attestation.unwrap().base_point_mul(),
        fixture.locking_points[2]
    );
}

#[test]
fn records_that_do_not_verify_are_refused() {
    let fixture = fixture();
    let record = fixture
        .announcement()
        .attested(fixture.attest(&[2]), &fixture.locking_points);

    let mut other_outcome = record.clone();
    other_outcome.winners = Some(vec![1]);
    assert_eq!(other_outcome.verify(), Err(RecordError::Attestation));

    let mut other_index = record.clone();
    other_index.outcome_index = Some(1);
    assert_eq!(other_index.verify(), Err(RecordError::Attestation));

    let mut forged = record.clone();
    forged.attestation = Some(MaybeScalar::from(dlctix::secp::Scalar::random(
        &mut rand::rng(),
    )));
    assert_eq!(forged.verify(), Err(RecordError::Attestation));

    let event = build_event(&fixture.key, &record, URL, Timestamp::from_secs(1)).unwrap();
    let mut edited = event.clone();
    edited.content = edited
        .content
        .replace("\"outcome_index\":2", "\"outcome_index\":1");
    assert_ne!(edited.content, event.content);
    assert_eq!(parse_event(&edited), Err(RecordError::Signature));
}

#[test]
fn announcements_carry_the_statement_and_no_outcome() {
    let fixture = fixture();
    let mut record = fixture.announcement();
    record.statement = Some(fixture.statement(fixture.nonce.point));
    let event = build_event(&fixture.key, &record, URL, Timestamp::from_secs(1)).unwrap();
    let parsed = parse_event(&event).unwrap();
    assert_eq!(parsed.stage(), Stage::Announced);
    assert_eq!(parsed.verify(), Ok(None));
    assert!(
        event
            .tags
            .iter()
            .any(|tag| tag.as_slice() == ["stage", "announced"])
    );

    let other_nonce = fixture.oracle.new_event_nonce(fixture.event_id).point;
    record.statement = Some(fixture.statement(other_nonce));
    assert_eq!(record.verify(), Err(RecordError::StatementMismatch));
    let mut signed = fixture.statement(fixture.nonce.point);
    signed.statement.signing_date += 1;
    record.statement = Some(signed);
    assert_eq!(record.verify(), Err(RecordError::Statement));
}

#[test]
fn the_publishing_key_is_its_own_persistent_key() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nostr.pem");
    let created = PublishingKey::load_or_create(&path).unwrap();
    let reloaded = PublishingKey::load_or_create(&path).unwrap();
    assert_eq!(created.public_key(), reloaded.public_key());
    let oracle = SigningKey::load_or_create(&directory.path().join("oracle.pem")).unwrap();
    assert_ne!(
        created.public_key().to_bytes(),
        oracle.x_only_public_key().serialize()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

/// Relays that refuse connections while listed in `down`.
#[derive(Default)]
struct FakeTransport {
    down: Mutex<HashSet<String>>,
    sent: Mutex<Vec<(String, NostrEvent)>>,
}

impl FakeTransport {
    fn set_down(&self, relay: &str, down: bool) {
        let mut set = self.down.lock().unwrap();
        if down {
            set.insert(relay.to_owned());
        } else {
            set.remove(relay);
        }
    }

    fn sent_to(&self, relay: &str) -> Vec<NostrEvent> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|(to, _)| to == relay)
            .map(|(_, event)| event.clone())
            .collect()
    }
}

impl Transport for FakeTransport {
    async fn publish(
        &self,
        relay: &str,
        events: &[NostrEvent],
    ) -> Result<Vec<Result<(), String>>, String> {
        if self.down.lock().unwrap().contains(relay) {
            return Err("connection refused".to_owned());
        }
        let mut sent = self.sent.lock().unwrap();
        sent.extend(events.iter().map(|event| (relay.to_owned(), event.clone())));
        Ok(vec![Ok(()); events.len()])
    }
}

#[derive(Default)]
struct FakeRecords(Mutex<HashMap<Uuid, Record>>);

impl Records for FakeRecords {
    async fn record(&self, event_id: Uuid) -> Result<Option<Record>, String> {
        Ok(self.0.lock().unwrap().get(&event_id).cloned())
    }
}

const A: &str = "wss://a.example.com";
const B: &str = "wss://b.example.com";

async fn queue(database: &Database, event_id: Uuid, stage: Stage, now: &AtomicI64) {
    database
        .queue_publication(
            event_id,
            stage,
            vec![A.to_owned(), B.to_owned()],
            now.load(Ordering::SeqCst),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_publications_are_retried_with_backoff() {
    let directory = tempfile::tempdir().unwrap();
    let (database, writer) = Database::open(directory.path()).await.unwrap();
    let shutdown = CancellationToken::new();
    let writer = tokio::spawn(writer.run(shutdown.clone()));

    let fixture = fixture();
    let records = Arc::new(FakeRecords::default());
    records
        .0
        .lock()
        .unwrap()
        .insert(fixture.event_id, fixture.announcement());
    let now = Arc::new(AtomicI64::new(SIGNING_DATE - 3_600));
    let clock: Clock = {
        let now = now.clone();
        Arc::new(move || OffsetDateTime::from_unix_timestamp(now.load(Ordering::SeqCst)).unwrap())
    };
    let advance = |seconds: i64| now.fetch_add(seconds, Ordering::SeqCst);
    let transport = FakeTransport::default();
    transport.set_down(B, true);
    let publisher = Publisher::new(
        database.clone(),
        records.clone(),
        PublishingKey::load_or_create(&directory.path().join("nostr.pem")).unwrap(),
        vec![A.to_owned(), B.to_owned()],
        "https://oracle.example.com".to_owned(),
        transport,
        clock,
    );

    queue(&database, fixture.event_id, Stage::Announced, &now).await;
    let summary = publisher.run_pass().await.unwrap();
    assert_eq!((summary.published, summary.failed, summary.rows), (1, 1, 2));
    assert_eq!(publisher.backlog().await.unwrap(), 1);
    assert_eq!(publisher.transport.sent_to(A).len(), 1);

    // The relay is back, but the row waits out its first backoff.
    publisher.transport.set_down(B, false);
    assert_eq!(publisher.run_pass().await.unwrap().rows, 0);
    advance(29);
    assert_eq!(publisher.run_pass().await.unwrap().rows, 0);
    advance(1);
    let summary = publisher.run_pass().await.unwrap();
    assert_eq!((summary.published, summary.failed), (1, 0));
    assert_eq!(publisher.backlog().await.unwrap(), 0);
    let announced = publisher.transport.sent_to(B).remove(0);
    assert_eq!(parse_event(&announced).unwrap().stage(), Stage::Announced);
    // Published rows are not sent again.
    queue(&database, fixture.event_id, Stage::Announced, &now).await;
    assert_eq!(publisher.run_pass().await.unwrap().rows, 0);

    // The attestation replaces the announcement: a newer created_at, even
    // within the same second. B is down again and backs off twice as long
    // after its second failure.
    records.0.lock().unwrap().insert(
        fixture.event_id,
        fixture
            .announcement()
            .attested(fixture.attest(&[0]), &fixture.locking_points),
    );
    publisher.transport.set_down(B, true);
    queue(&database, fixture.event_id, Stage::Attested, &now).await;
    let summary = publisher.run_pass().await.unwrap();
    assert_eq!((summary.published, summary.failed), (1, 1));
    let attested = publisher.transport.sent_to(A).pop().unwrap();
    assert!(attested.created_at > announced.created_at);
    assert_eq!(parse_event(&attested).unwrap().verify(), Ok(Some(0)));
    advance(30);
    assert_eq!(publisher.run_pass().await.unwrap().failed, 1);
    advance(59);
    assert_eq!(publisher.run_pass().await.unwrap().rows, 0);
    publisher.transport.set_down(B, false);
    advance(1);
    assert_eq!(publisher.run_pass().await.unwrap().published, 1);
    assert_eq!(publisher.backlog().await.unwrap(), 0);
    assert_eq!(
        parse_event(&publisher.transport.sent_to(B).pop().unwrap())
            .unwrap()
            .stage(),
        Stage::Attested
    );

    // Rows of an event that no longer exists are dropped.
    let missing = Uuid::now_v7();
    queue(&database, missing, Stage::Announced, &now).await;
    assert_eq!(publisher.run_pass().await.unwrap().published, 0);
    assert_eq!(publisher.backlog().await.unwrap(), 0);

    shutdown.cancel();
    writer.await.unwrap().unwrap();
}
