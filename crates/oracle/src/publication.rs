//! Publishes event announcements and attestations to Nostr relays, so an
//! attestation outlives the oracle and the coordinators that used it. The
//! event format is in `docs/NOSTR.md`.
//!
//! - Events are signed by a dedicated Nostr key, never the attestation key.
//!   The record inside carries its own proof: the attestation verifies
//!   against the attestation key and nonce point it names.
//! - Creating or attesting an event queues one outbox row per relay in the
//!   event database, after the event write commits. A background publisher
//!   sends due rows and retries failures with backoff, so nothing on the
//!   request or attestation path waits on a relay.
//! - Each publication carries the event's current state under one `d` tag,
//!   so the attested record replaces the announcement on relays.

mod relay;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use dlctix::{
    attestation_locking_point,
    bitcoin::hex::DisplayHex,
    musig2::secp256k1::PublicKey as SecpPublicKey,
    secp::{MaybePoint, MaybeScalar, Point},
};
use log::warn;
use nostr::{
    event::{Event as NostrEvent, EventBuilder, FinalizeEvent, Kind, Tag},
    key::{Keys, PublicKey as NostrPublicKey, SecretKey as NostrSecretKey},
    types::Timestamp,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use uuid::Uuid;
use zeroize::Zeroizing;

pub use relay::{Transport, WebSocketTransport};

use crate::{
    database::{Database, DuePublication},
    events::Event,
    oracle::{Clock, Error as OracleError, Oracle},
    scoring,
    signing::{KeyError, load_or_create_secret},
    statement::SignedStatement,
};

/// NIP-78 application data. Addressable by author, kind, and `d` tag, so a
/// newer state of an event replaces the older one on relays.
pub const KIND: u16 = 30078;
/// `type` of the record in the event content.
pub const RECORD_TYPE: &str = "oracle-event";
pub const RECORD_VERSION: u32 = 1;
/// Rows sent per pass; passes are spaced by the publisher, which bounds
/// how fast a backfill reaches the relays.
pub const PASS_ROWS: usize = 20;

/// The `d` tag of an event's record.
pub fn identifier(event_id: Uuid) -> String {
    format!("oracle:{event_id}")
}

/// What a publication follows: the event's creation or its attestation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    Announced,
    Attested,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Announced => "announced",
            Stage::Attested => "attested",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "announced" => Some(Stage::Announced),
            "attested" => Some(Stage::Attested),
            _ => None,
        }
    }
}

/// The oracle's Nostr publishing key. Kept in its own file and refused
/// when it equals the attestation key.
pub struct PublishingKey {
    keys: Keys,
}

impl PublishingKey {
    /// Loads the key at `path`, or creates it with mode 0600 when missing.
    pub fn load_or_create(path: &Path) -> Result<Self, KeyError> {
        let mut secret = load_or_create_secret(path)?;
        let bytes = Zeroizing::new(secret.secret_bytes());
        secret.non_secure_erase();
        let secret =
            NostrSecretKey::from_slice(bytes.as_slice()).map_err(|_| KeyError::InvalidKey {
                path: path.display().to_string(),
            })?;
        Ok(Self {
            keys: Keys::new(secret),
        })
    }

    pub fn public_key(&self) -> NostrPublicKey {
        self.keys.public_key()
    }
}

/// Where the oracle queues publications, when publishing is on.
#[derive(Clone)]
pub struct Publication {
    pub relays: Vec<String>,
    pub public_key: NostrPublicKey,
    /// Wakes the publisher after a row is queued.
    pub wake: Arc<Notify>,
}

/// How an event lists its outcomes. Mirrors the statement's `outcomes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcomes {
    /// Outcome `i` is the `i`th of every ordered choice of `places` winners
    /// from `0..entries`, then the refund-all outcome `[0, 1, …, entries − 1]`.
    Ranking { entries: usize, places: usize },
}

/// The content of a published event: an announcement, plus the attestation
/// once the event is signed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub v: u32,
    #[serde(rename = "type")]
    pub record_type: String,
    pub event_id: Uuid,
    /// The attestation key `P`, compressed hex. Not the Nostr author.
    pub oracle_pubkey: Point,
    /// Public nonce point `R` the attestation is made with
    pub nonce_point: Point,
    /// When the oracle attests, UNIX seconds
    pub signing_date: i64,
    /// DLC expiry, UNIX seconds
    pub expiry: Option<u32>,
    pub outcomes: Outcomes,
    /// SHA-256 of the announced locking points, each 33 bytes compressed,
    /// in outcome order. Lets a reader check the list it derives.
    pub locking_points_sha256: String,
    /// The oracle's signed statement, once every entry is in
    pub statement: Option<SignedStatement>,
    /// The attestation `s`, once signed
    pub attestation: Option<MaybeScalar>,
    /// The attested outcome's index, once signed
    pub outcome_index: Option<usize>,
    /// The attested outcome's winners, once signed
    pub winners: Option<Vec<usize>>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("the Nostr event signature or id does not verify")]
    Signature,
    #[error("kind {0} is not an oracle record")]
    Kind(u16),
    #[error("the content is not an oracle record: {0}")]
    Content(String),
    #[error("unsupported record version {0}")]
    Version(u32),
    #[error("the d tag does not name the record's event")]
    Identifier,
    #[error("the statement signature does not verify against the oracle key")]
    Statement,
    #[error("the statement is for another event or nonce point")]
    StatementMismatch,
    #[error("the attestation does not unlock the stated outcome")]
    Attestation,
}

impl Record {
    /// An announcement of `event_id` with these locking points.
    pub fn announcement(
        event_id: Uuid,
        oracle_pubkey: Point,
        nonce_point: Point,
        signing_date: i64,
        expiry: Option<u32>,
        outcomes: Outcomes,
        locking_points: &[MaybePoint],
    ) -> Self {
        let mut digest = Sha256::new();
        for point in locking_points {
            digest.update(point.serialize());
        }
        let digest: [u8; 32] = digest.finalize().into();
        Self {
            v: RECORD_VERSION,
            record_type: RECORD_TYPE.to_owned(),
            event_id,
            oracle_pubkey,
            nonce_point,
            signing_date,
            expiry,
            outcomes,
            locking_points_sha256: digest.to_lower_hex_string(),
            statement: None,
            attestation: None,
            outcome_index: None,
            winners: None,
        }
    }

    /// Adds the attestation and the outcome among `locking_points` it unlocks.
    pub fn attested(mut self, attestation: MaybeScalar, locking_points: &[MaybePoint]) -> Self {
        let unlocked = attestation.base_point_mul();
        let Outcomes::Ranking { entries, places } = self.outcomes;
        self.outcome_index = locking_points.iter().position(|point| *point == unlocked);
        self.winners = self.outcome_index.and_then(|index| {
            scoring::ranking_outcomes(entries, places)
                .into_iter()
                .nth(index)
        });
        self.attestation = Some(attestation);
        self
    }

    /// The record of `event` as the oracle serves it.
    pub fn from_event(event: &Event, oracle_pubkey: Point) -> Self {
        let locking_points = &event.event_announcement.locking_points;
        let mut record = Self::announcement(
            event.id,
            oracle_pubkey,
            event.nonce_point,
            event.signing_date.unix_timestamp(),
            event.event_announcement.expiry,
            Outcomes::Ranking {
                entries: usize::try_from(event.total_allowed_entries).unwrap_or(0),
                places: usize::try_from(event.number_of_places_win).unwrap_or(0),
            },
            locking_points,
        );
        record.statement = event.statement.clone();
        match event.attestation {
            Some(attestation) => record.attested(attestation, locking_points),
            None => record,
        }
    }

    pub fn stage(&self) -> Stage {
        if self.attestation.is_some() {
            Stage::Attested
        } else {
            Stage::Announced
        }
    }

    /// Checks the statement's signature, then that the attestation is the
    /// oracle's signature on the stated outcome: `s·G` equals the locking
    /// point of the winners' message under `P` and `R`, as the oracle checks
    /// when it attests. Returns the attested outcome's index.
    pub fn verify(&self) -> Result<Option<usize>, RecordError> {
        if let Some(signed) = &self.statement {
            let key = SecpPublicKey::from_slice(&self.oracle_pubkey.serialize())
                .map_err(|_| RecordError::Statement)?;
            signed
                .verify(&key.x_only_public_key().0)
                .map_err(|_| RecordError::Statement)?;
            if signed.statement.event_id != self.event_id
                || signed.statement.nonce_point != self.nonce_point
            {
                return Err(RecordError::StatementMismatch);
            }
        }
        let Some(attestation) = self.attestation else {
            return Ok(None);
        };
        let (Some(index), Some(winners)) = (self.outcome_index, &self.winners) else {
            return Err(RecordError::Attestation);
        };
        let Outcomes::Ranking { entries, places } = self.outcomes;
        if scoring::ranking_outcomes(entries, places).get(index) != Some(winners) {
            return Err(RecordError::Attestation);
        }
        let locking_point = attestation_locking_point(
            self.oracle_pubkey,
            self.nonce_point,
            scoring::outcome_message(winners),
        );
        if !matches!(locking_point, MaybePoint::Valid(_))
            || attestation.base_point_mul() != locking_point
        {
            return Err(RecordError::Attestation);
        }
        Ok(Some(index))
    }
}

/// Signs `record` as a kind 30078 event. `url` is where the oracle serves
/// the event.
pub fn build_event(
    key: &PublishingKey,
    record: &Record,
    url: &str,
    created_at: Timestamp,
) -> Result<NostrEvent, String> {
    let content = serde_json::to_string(record).map_err(|error| error.to_string())?;
    EventBuilder::new(Kind::Custom(KIND), content)
        .tags([
            Tag::identifier(identifier(record.event_id)),
            Tag::custom(
                "oracle",
                [record.oracle_pubkey.serialize().to_lower_hex_string()],
            ),
            Tag::custom("stage", [record.stage().as_str()]),
            Tag::custom("r", [url]),
            Tag::custom(
                "alt",
                [format!("DLC oracle {} event", record.stage().as_str())],
            ),
        ])
        .custom_created_at(created_at)
        .finalize(&key.keys)
        .map_err(|error| error.to_string())
}

/// Reads the record in a published event, after checking the event's id
/// and signature. The caller still checks the record ([`Record::verify`])
/// and that `oracle_pubkey` is the oracle it expects.
pub fn parse_event(event: &NostrEvent) -> Result<Record, RecordError> {
    event.verify().map_err(|_| RecordError::Signature)?;
    if event.kind != Kind::Custom(KIND) {
        return Err(RecordError::Kind(event.kind.as_u16()));
    }
    let record: Record = serde_json::from_str(&event.content)
        .map_err(|error| RecordError::Content(error.to_string()))?;
    if record.record_type != RECORD_TYPE {
        return Err(RecordError::Content(format!("type {}", record.record_type)));
    }
    if record.v != RECORD_VERSION {
        return Err(RecordError::Version(record.v));
    }
    if event.tags.identifier() != Some(identifier(record.event_id)) {
        return Err(RecordError::Identifier);
    }
    Ok(record)
}

/// Reads the current record of an event.
pub trait Records: Send + Sync {
    /// `None` when the event does not exist.
    fn record(&self, event_id: Uuid)
    -> impl Future<Output = Result<Option<Record>, String>> + Send;
}

impl Records for Oracle {
    async fn record(&self, event_id: Uuid) -> Result<Option<Record>, String> {
        match self.get_event(event_id).await {
            Ok(event) => Ok(Some(Record::from_event(&event, self.public_key()))),
            Err(OracleError::EventNotFound(_)) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// What one pass did, counted per relay delivery.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassSummary {
    pub published: u64,
    pub failed: u64,
    /// Rows the pass took; [`PASS_ROWS`] means more may be due.
    pub rows: usize,
}

/// Sends due outbox rows to their relays.
pub struct Publisher<T, R> {
    database: Database,
    records: Arc<R>,
    key: PublishingKey,
    relays: Vec<String>,
    /// The oracle's public origin; records link to `/oracle/events/{id}`.
    remote_url: String,
    transport: T,
    clock: Clock,
}

impl<T: Transport, R: Records> Publisher<T, R> {
    pub fn new(
        database: Database,
        records: Arc<R>,
        key: PublishingKey,
        relays: Vec<String>,
        remote_url: String,
        transport: T,
        clock: Clock,
    ) -> Self {
        Self {
            database,
            records,
            key,
            relays,
            remote_url,
            transport,
            clock,
        }
    }

    fn now(&self) -> i64 {
        (self.clock)().unix_timestamp()
    }

    /// Queues events whose signing date is at most `days` ago for every
    /// relay. Rows already published are not sent again.
    pub async fn queue_recent(&self, days: i64) -> Result<u64, String> {
        let now = self.now();
        self.database
            .queue_recent_publications(
                now.saturating_sub(days.saturating_mul(86_400)),
                self.relays.clone(),
                now,
            )
            .await
            .map_err(|error| error.to_string())
    }

    /// Unpublished rows for the configured relays.
    pub async fn backlog(&self) -> Result<u64, String> {
        self.database
            .publication_backlog(&self.relays)
            .await
            .map_err(|error| error.to_string())
    }

    /// Sends up to [`PASS_ROWS`] due rows: one Nostr event per event with
    /// its current state, to every relay it is due on, relays in parallel.
    /// A failure is recorded on its rows and retried later.
    pub async fn run_pass(&self) -> Result<PassSummary, String> {
        let now = self.now();
        let due = self
            .database
            .due_publications(&self.relays, now, PASS_ROWS)
            .await
            .map_err(|error| error.to_string())?;
        let mut summary = PassSummary {
            rows: due.len(),
            ..PassSummary::default()
        };
        // Stages due per event and relay; the event's current record
        // covers all of them.
        let mut deliveries: BTreeMap<(String, Uuid), Vec<Stage>> = BTreeMap::new();
        for DuePublication {
            event_id,
            stage,
            relay,
        } in due
        {
            deliveries.entry((relay, event_id)).or_default().push(stage);
        }
        let event_ids: BTreeSet<Uuid> = deliveries.keys().map(|(_, id)| *id).collect();
        let mut built: BTreeMap<Uuid, Result<NostrEvent, String>> = BTreeMap::new();
        for event_id in event_ids {
            match self.build(event_id, now).await {
                Ok(Some(event)) => {
                    built.insert(event_id, Ok(event));
                }
                Ok(None) => {
                    if let Err(error) = self.database.drop_publications(event_id).await {
                        warn!("nostr: cannot drop rows of missing event {event_id}: {error}");
                    }
                    deliveries.retain(|(_, id), _| *id != event_id);
                }
                Err(error) => {
                    built.insert(event_id, Err(error));
                }
            }
        }

        let mut per_relay: BTreeMap<String, Vec<(Uuid, Vec<Stage>)>> = BTreeMap::new();
        for ((relay, event_id), stages) in deliveries {
            per_relay.entry(relay).or_default().push((event_id, stages));
        }
        let sends = per_relay.into_iter().map(|(relay, items)| {
            let built = &built;
            async move {
                let ready: Vec<(Uuid, &NostrEvent)> = items
                    .iter()
                    .filter_map(|(id, _)| match built.get(id) {
                        Some(Ok(event)) => Some((*id, event)),
                        _ => None,
                    })
                    .collect();
                let events: Vec<NostrEvent> =
                    ready.iter().map(|(_, event)| (*event).clone()).collect();
                let results = if events.is_empty() {
                    Ok(vec![])
                } else {
                    self.transport.publish(&relay, &events).await
                };
                (relay, items, ready, results)
            }
        });
        for (relay, items, ready, results) in futures::future::join_all(sends).await {
            for (event_id, stages) in items {
                let outcome = match built.get(&event_id) {
                    Some(Err(error)) => Err(format!("cannot build the event: {error}")),
                    _ => match &results {
                        Err(error) => Err(error.clone()),
                        Ok(results) => ready
                            .iter()
                            .position(|(id, _)| *id == event_id)
                            .and_then(|position| results.get(position).cloned())
                            .unwrap_or_else(|| Err("no reply from the relay".to_owned())),
                    },
                };
                let recorded = match outcome {
                    Ok(()) => {
                        summary.published += 1;
                        let Some(Ok(event)) = built.get(&event_id) else {
                            continue;
                        };
                        self.database
                            .publication_sent(
                                event_id,
                                relay.clone(),
                                stages,
                                event.id.to_hex(),
                                i64::try_from(event.created_at.as_secs()).unwrap_or(i64::MAX),
                                now,
                            )
                            .await
                    }
                    Err(error) => {
                        summary.failed += 1;
                        warn!("nostr: event {event_id} not published to {relay}: {error}");
                        self.database
                            .publication_failed(event_id, relay.clone(), stages, error, now)
                            .await
                    }
                };
                if let Err(error) = recorded {
                    warn!("nostr: cannot record the outcome for event {event_id}: {error}");
                }
            }
        }
        Ok(summary)
    }

    /// The signed Nostr event with `event_id`'s current record, or `None`
    /// when the event is gone. Its `created_at` follows any earlier
    /// publication, so relays keep the newest state.
    async fn build(&self, event_id: Uuid, now: i64) -> Result<Option<NostrEvent>, String> {
        let Some(record) = self.records.record(event_id).await? else {
            return Ok(None);
        };
        let latest = self
            .database
            .latest_publication(event_id)
            .await
            .map_err(|error| error.to_string())?;
        let created_at = latest.map_or(now, |latest| now.max(latest.saturating_add(1)));
        let url = format!("{}/oracle/events/{event_id}", self.remote_url);
        build_event(
            &self.key,
            &record,
            &url,
            Timestamp::from_secs(u64::try_from(created_at).unwrap_or(0)),
        )
        .map(Some)
    }
}

#[cfg(test)]
mod tests;
