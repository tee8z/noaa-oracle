//! Statements about an event, signed by the oracle.
//!
//! A DLC's locking points depend only on the oracle key, the event's nonce point, and each
//! outcome's message; a message names no event. A party that checks a contract without seeing its
//! event, such as a verifier inside an enclave, could otherwise be handed another event's nonce
//! point, for example one already attested. A statement binds the nonce point to the event's id,
//! outcomes, and terms, so that party can check them against what it expects and derive the
//! locking points itself.
//!
//! A statement has three layers:
//!
//! - The attestation core every DLC on the event needs: the event id, when it is attested, its
//!   expiry, and its nonce point.
//! - [`Outcomes`]: how the outcomes are listed and which message each one attests. Competitions
//!   rank their entries; other contracts, such as parametric cover, will list tiers of a value.
//! - [`Terms`]: what the event measures and how it is judged, such as a prediction game scored
//!   against observed weather, tides, or space weather.
//!
//! Both enums are non-exhaustive, and the kind of each is part of the signed bytes, so later kinds
//! extend a statement without changing how existing ones are read.
//!
//! The oracle signs [`Statement::digest`] with BIP340, using its attestation key and no
//! auxiliary randomness, so a statement always gets the same signature. The encoding is in
//! `docs/attestation.md`.

use dlctix::{
    attestation_locking_point,
    musig2::secp256k1::{Secp256k1, XOnlyPublicKey, schnorr::Signature},
    secp::{MaybePoint, Point},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::str::FromStr;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    Event,
    scoring::{self, ScoringRules},
};

/// Domain tag of the statement's BIP340 tagged hash.
pub const STATEMENT_TAG: &[u8] = b"noaa-oracle/statement/v1";

/// What the oracle attests about one event, as it signs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    pub event_id: Uuid,
    /// When the oracle attests the outcome, UNIX seconds, rounded down
    pub signing_date: i64,
    /// The DLC expiry the announcement carries, UNIX seconds
    pub expiry: u32,
    /// Public nonce point `R` the attestation will be made with
    #[schema(value_type = String)]
    pub nonce_point: Point,
    /// How the outcomes are listed, and which message each attests
    pub outcomes: Outcomes,
    /// What the event measures, and how it is judged
    pub terms: Terms,
}

/// How an event's outcomes are listed. Clients must refuse a kind they do not know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Outcomes {
    /// Every ordered choice of winners among the entries, then the refund-all outcome
    Ranking(RankingOutcomes),
}

/// The outcomes of a competition that ranks its entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RankingOutcomes {
    pub number_of_places_win: u32,
    /// Every entry, in id order: entry `i` is outcome index `i`
    pub entry_ids: Vec<Uuid>,
}

/// What an event measures, and how it is judged. Clients must refuse a kind they do not know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Terms {
    /// Predictions scored against values observed over a window, such as NOAA weather
    Observation(ObservationTerms),
}

/// The terms of an event scored against observed values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservationTerms {
    pub source: String,
    /// UNIX seconds, rounded down
    pub start_observation_date: i64,
    /// UNIX seconds, rounded down
    pub end_observation_date: i64,
    /// What the source observes: NOAA station ids for `noaa_weather`
    pub targets: Vec<String>,
    pub scoring_fields: Vec<String>,
    pub number_of_values_per_entry: u32,
    /// How picks score
    pub scoring_rules: ScoringRules,
    /// For `lines` rules, the line each target and metric scores against, fixed when the event
    /// was created, sorted by target then metric; empty for `fixed` rules
    pub lines: Vec<LineTerms>,
}

/// A line an event scores one target and metric against: `Under` below `lower`, `Over` above
/// `upper`, `Par` from `lower` to `upper`, both included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LineTerms {
    pub target: String,
    pub metric: String,
    /// On the miss `observed - baseline`, in the metric's unit
    pub lower: f64,
    pub upper: f64,
    /// Length of the past windows the line was fitted on
    pub window_hours: u32,
}

/// A [`Statement`] and the oracle's signature over its digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SignedStatement {
    pub statement: Statement,
    /// BIP340 signature by the oracle key over [`Statement::digest`], hex
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StatementError {
    #[error("the statement signature is not a BIP340 signature")]
    Malformed,
    #[error("the statement is not signed by this oracle key")]
    BadSignature,
}

impl Statement {
    /// The statement for `event`, once every entry has been submitted.
    pub fn for_event(event: &Event) -> Option<Self> {
        let count = |value: i64| u32::try_from(value).ok();
        if event.entry_ids.len() != usize::try_from(event.total_allowed_entries).ok()? {
            return None;
        }
        let mut entry_ids = event.entry_ids.clone();
        entry_ids.sort_unstable();
        Some(Self {
            event_id: event.id,
            signing_date: event.signing_date.unix_timestamp(),
            expiry: event.event_announcement.expiry?,
            nonce_point: event.nonce_point,
            outcomes: Outcomes::Ranking(RankingOutcomes {
                number_of_places_win: count(event.number_of_places_win)?,
                entry_ids,
            }),
            terms: Terms::Observation(ObservationTerms {
                source: event.source.clone(),
                start_observation_date: event.start_observation_date.unix_timestamp(),
                end_observation_date: event.end_observation_date.unix_timestamp(),
                targets: event.locations.clone(),
                scoring_fields: event.scoring_fields.clone(),
                number_of_values_per_entry: count(event.number_of_values_per_entry)?,
                scoring_rules: event.scoring_rules,
                lines: {
                    let mut lines = event
                        .lines
                        .iter()
                        .map(|line| {
                            Some(LineTerms {
                                target: line.target.clone(),
                                metric: line.metric.clone(),
                                lower: line.lower,
                                upper: line.upper,
                                window_hours: u32::try_from(line.window_hours).ok()?,
                            })
                        })
                        .collect::<Option<Vec<_>>>()?;
                    lines.sort_by(|a, b| (&a.target, &a.metric).cmp(&(&b.target, &b.metric)));
                    lines
                },
            }),
        })
    }

    /// The message each outcome attests, in announcement order.
    pub fn outcome_messages(&self) -> Vec<Vec<u8>> {
        match &self.outcomes {
            Outcomes::Ranking(ranking) => scoring::ranking_outcomes(
                ranking.entry_ids.len(),
                ranking.number_of_places_win as usize,
            )
            .iter()
            .map(|winners| scoring::outcome_message(winners))
            .collect(),
        }
    }

    /// The locking points of every outcome under `oracle`, in announcement order.
    pub fn locking_points(&self, oracle: impl Into<Point>) -> Vec<MaybePoint> {
        let oracle = oracle.into();
        self.outcome_messages()
            .iter()
            .map(|message| attestation_locking_point(oracle, self.nonce_point, message))
            .collect()
    }

    /// The bytes the digest commits to: the core, then each part's kind and fields. See
    /// `docs/attestation.md`.
    pub fn message(&self) -> Vec<u8> {
        let mut message = Vec::with_capacity(256);
        message.extend_from_slice(self.event_id.as_bytes());
        message.extend_from_slice(&self.signing_date.to_be_bytes());
        message.extend_from_slice(&self.expiry.to_be_bytes());
        message.extend_from_slice(&self.nonce_point.serialize());
        match &self.outcomes {
            Outcomes::Ranking(ranking) => {
                put_string(&mut message, "ranking");
                message.extend_from_slice(&ranking.number_of_places_win.to_be_bytes());
                put_count(&mut message, ranking.entry_ids.len());
                for entry in &ranking.entry_ids {
                    message.extend_from_slice(entry.as_bytes());
                }
            }
        }
        match &self.terms {
            Terms::Observation(terms) => {
                put_string(&mut message, "observation");
                put_string(&mut message, &terms.source);
                message.extend_from_slice(&terms.start_observation_date.to_be_bytes());
                message.extend_from_slice(&terms.end_observation_date.to_be_bytes());
                put_strings(&mut message, &terms.targets);
                put_strings(&mut message, &terms.scoring_fields);
                message.extend_from_slice(&terms.number_of_values_per_entry.to_be_bytes());
                put_string(&mut message, terms.scoring_rules.as_str());
                put_count(&mut message, terms.lines.len());
                for line in &terms.lines {
                    put_string(&mut message, &line.target);
                    put_string(&mut message, &line.metric);
                    message.extend_from_slice(&line.lower.to_be_bytes());
                    message.extend_from_slice(&line.upper.to_be_bytes());
                    message.extend_from_slice(&line.window_hours.to_be_bytes());
                }
            }
        }
        message
    }

    /// The BIP340 tagged hash of [`Self::message`] under [`STATEMENT_TAG`].
    pub fn digest(&self) -> [u8; 32] {
        let tag = Sha256::digest(STATEMENT_TAG);
        Sha256::new()
            .chain_update(tag)
            .chain_update(tag)
            .chain_update(self.message())
            .finalize()
            .into()
    }
}

impl SignedStatement {
    /// Checks the signature against the oracle's x-only public key.
    pub fn verify(&self, oracle: &XOnlyPublicKey) -> Result<(), StatementError> {
        let signature =
            Signature::from_str(&self.signature).map_err(|_| StatementError::Malformed)?;
        Secp256k1::verification_only()
            .verify_schnorr(&signature, &self.statement.digest(), oracle)
            .map_err(|_| StatementError::BadSignature)
    }
}

/// Lengths and counts are bounded far below `u16::MAX` by event validation.
fn put_count(message: &mut Vec<u8>, count: usize) {
    let count = u16::try_from(count).expect("event validation bounds every count");
    message.extend_from_slice(&count.to_be_bytes());
}

fn put_string(message: &mut Vec<u8>, value: &str) {
    put_count(message, value.len());
    message.extend_from_slice(value.as_bytes());
}

fn put_strings(message: &mut Vec<u8>, values: &[String]) {
    put_count(message, values.len());
    for value in values {
        put_string(message, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing::SigningKey;
    use dlctix::bitcoin::hex::DisplayHex;

    fn statement() -> Statement {
        let start = 1_790_000_000;
        Statement {
            event_id: Uuid::from_str("01926f3a-0000-7000-8000-0000000000aa").unwrap(),
            signing_date: start + 2 * 86_400,
            expiry: (start + 3 * 86_400) as u32,
            // 2·G
            nonce_point: Point::from_hex(
                "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
            )
            .unwrap(),
            outcomes: Outcomes::Ranking(RankingOutcomes {
                number_of_places_win: 1,
                entry_ids: (1..=3)
                    .map(|entry| Uuid::from_u128(0x01926f3a_0000_7000_8000_000000000000 | entry))
                    .collect(),
            }),
            terms: Terms::Observation(ObservationTerms {
                source: "noaa_weather".into(),
                start_observation_date: start,
                end_observation_date: start + 86_400,
                targets: vec!["KORD".into(), "KSAW".into()],
                scoring_fields: vec!["temp_high".into(), "temp_low".into(), "wind_speed".into()],
                number_of_values_per_entry: 3,
                scoring_rules: ScoringRules::Lines,
                lines: vec![
                    LineTerms {
                        target: "KORD".into(),
                        metric: "temp_high".into(),
                        lower: -2.5,
                        upper: -0.5,
                        window_hours: 24,
                    },
                    LineTerms {
                        target: "KSAW".into(),
                        metric: "wind_speed".into(),
                        lower: 1.5,
                        upper: 4.5,
                        window_hours: 24,
                    },
                ],
            }),
        }
    }

    fn ranking(statement: &mut Statement) -> &mut RankingOutcomes {
        match &mut statement.outcomes {
            Outcomes::Ranking(ranking) => ranking,
        }
    }

    fn observation(statement: &mut Statement) -> &mut ObservationTerms {
        match &mut statement.terms {
            Terms::Observation(terms) => terms,
        }
    }

    fn new_key() -> (tempfile::TempDir, SigningKey) {
        let directory = tempfile::tempdir().unwrap();
        let key = SigningKey::load_or_create(&directory.path().join("oracle.pem")).unwrap();
        (directory, key)
    }

    /// The vector comes from an independent Python implementation of `docs/attestation.md`.
    #[test]
    fn digest_matches_the_reference_vector() {
        assert_eq!(statement().message().len(), MESSAGE_LENGTH);
        assert_eq!(statement().digest().to_lower_hex_string(), DIGEST);
    }

    #[test]
    fn every_field_changes_the_digest() {
        type Change = fn(&mut Statement);
        let changes: &[(&str, Change)] = &[
            ("event id", |s| s.event_id = Uuid::now_v7()),
            ("signing date", |s| s.signing_date += 1),
            ("expiry", |s| s.expiry += 1),
            ("nonce point", |s| s.nonce_point = Point::generator()),
            ("places", |s| ranking(s).number_of_places_win = 2),
            ("entry order", |s| ranking(s).entry_ids.swap(0, 1)),
            ("entries", |s| ranking(s).entry_ids[2] = Uuid::now_v7()),
            ("entry count", |s| ranking(s).entry_ids.truncate(2)),
            ("source", |s| observation(s).source = "space_weather".into()),
            ("start", |s| observation(s).start_observation_date += 1),
            ("end", |s| observation(s).end_observation_date += 1),
            ("targets", |s| observation(s).targets.reverse()),
            ("scoring fields", |s| {
                observation(s).scoring_fields.truncate(2)
            }),
            ("values per entry", |s| {
                observation(s).number_of_values_per_entry = 2
            }),
            ("scoring rules", |s| {
                let terms = observation(s);
                terms.scoring_rules = ScoringRules::Fixed;
                terms.lines.clear();
            }),
            ("line lower", |s| observation(s).lines[0].lower = -2.0),
            ("line upper", |s| observation(s).lines[1].upper = 5.0),
            ("line window", |s| observation(s).lines[0].window_hours = 48),
            ("line metric", |s| {
                observation(s).lines[1].metric = "temp_low".into()
            }),
            ("line order", |s| observation(s).lines.swap(0, 1)),
            // Length prefixes keep adjacent strings from running together.
            ("string boundary", |s| {
                observation(s).targets = vec!["KORDK".into(), "SAW".into()]
            }),
        ];
        for (name, change) in changes {
            let mut changed = statement();
            change(&mut changed);
            assert_ne!(changed.digest(), statement().digest(), "{name}");
        }
    }

    /// The statement alone reproduces the announcement the oracle makes for its event.
    #[test]
    fn locking_points_follow_from_the_statement() {
        let (_directory, key) = new_key();
        let statement = statement();
        let points = statement.locking_points(key.public_key());
        // Three entries and one place: three winners, then refund-all.
        assert_eq!(points.len(), 4);
        for (point, message) in points.iter().zip(statement.outcome_messages()) {
            assert_eq!(*point, key.locking_point(statement.nonce_point, &message));
        }
    }

    #[test]
    fn signatures_verify_only_for_the_statement_and_key() {
        let (_directory, key) = new_key();
        let signed = key.sign_statement(statement());
        assert_eq!(signed.verify(&key.x_only_public_key()), Ok(()));
        assert_eq!(
            key.sign_statement(statement()),
            signed,
            "a statement always gets the same signature"
        );

        let (_other_directory, other) = new_key();
        assert_eq!(
            signed.verify(&other.x_only_public_key()),
            Err(StatementError::BadSignature)
        );
        let mut changed = signed.clone();
        ranking(&mut changed.statement).entry_ids.swap(0, 2);
        assert_eq!(
            changed.verify(&key.x_only_public_key()),
            Err(StatementError::BadSignature)
        );
        let mut truncated = signed;
        truncated.signature.truncate(126);
        assert_eq!(
            truncated.verify(&key.x_only_public_key()),
            Err(StatementError::Malformed)
        );
    }

    /// Each part carries its kind, and a client refuses a kind or field it does not know.
    #[test]
    fn parts_are_tagged_by_kind() {
        let (_directory, key) = new_key();
        let signed = key.sign_statement(statement());
        let json = serde_json::to_value(&signed).unwrap();
        assert_eq!(json["statement"]["outcomes"]["kind"], "ranking");
        assert_eq!(json["statement"]["terms"]["kind"], "observation");
        assert_eq!(json["statement"]["terms"]["source"], "noaa_weather");
        let read: SignedStatement = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(read, signed);

        type Change = fn(&mut serde_json::Value);
        let refused: &[(&str, Change)] = &[
            ("unknown outcomes", |j| {
                j["statement"]["outcomes"]["kind"] = "tiers".into()
            }),
            ("unknown terms", |j| {
                j["statement"]["terms"]["kind"] = "tides".into()
            }),
            ("extra outcome field", |j| {
                j["statement"]["outcomes"]["payout"] = 3.into()
            }),
            ("extra term field", |j| {
                j["statement"]["terms"]["tide_height"] = 3.into()
            }),
            ("extra core field", |j| j["statement"]["premium"] = 3.into()),
        ];
        for (name, change) in refused {
            let mut candidate = json.clone();
            change(&mut candidate);
            assert!(
                serde_json::from_value::<SignedStatement>(candidate).is_err(),
                "{name}"
            );
        }
    }

    const MESSAGE_LENGTH: usize = 304;
    const DIGEST: &str = "2b37e13098210b2216a8b101d8a6e16420101c9acb954c1a53d43c0e990303f4";
}
