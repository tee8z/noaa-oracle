//! Source-independent scoring, ranking, and outcome encoding.
//!
//! The outcome list is a contract with the coordinator, which rebuilds it
//! with the same `(entries, places)` permutation order to assign payouts by
//! outcome index. Entry index `i` is the `i`-th entry in id order.

use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::cmp::{Ordering, Reverse};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    events::ValueOptions,
    sources::{Metric, ParRule, Reading},
};

pub const OVER_OR_UNDER_POINTS: u64 = 10;
pub const PAR_POINTS: u64 = 20;

/// One prediction in an entry: `prediction` for `metric` at `target`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Pick {
    pub target: String,
    pub metric: String,
    pub prediction: ValueOptions,
}

/// Points for one prediction. Returns 0 when either value is missing.
pub fn points(prediction: &ValueOptions, rule: ParRule, reading: &Reading) -> u64 {
    let (Some(mut baseline), Some(mut observed)) = (reading.baseline, reading.observed) else {
        return 0;
    };
    if rule == ParRule::Rounded {
        baseline = baseline.round();
        observed = observed.round();
    }
    let par = match rule {
        ParRule::Exact | ParRule::Rounded => observed == baseline,
        ParRule::Within(tolerance) => (observed - baseline).abs() <= tolerance,
        ParRule::Compass(tolerance) => {
            let difference = (observed - baseline).abs() % 360.0;
            difference.min(360.0 - difference) <= tolerance
        }
    };
    match (prediction, observed.partial_cmp(&baseline)) {
        (ValueOptions::Par, _) if par => PAR_POINTS,
        (ValueOptions::Over, Some(Ordering::Greater))
        | (ValueOptions::Under, Some(Ordering::Less)) => OVER_OR_UNDER_POINTS,
        _ => 0,
    }
}

/// Base score of an entry: the sum of its picks' points, counting only
/// metrics enabled for the event.
pub fn base_score(
    picks: &[Pick],
    readings: &[Reading],
    enabled: &[String],
    metric: impl Fn(&str) -> Option<Metric>,
) -> u64 {
    picks
        .iter()
        .filter(|pick| enabled.contains(&pick.metric))
        .filter_map(|pick| {
            let rule = metric(&pick.metric)?.par;
            let reading = readings
                .iter()
                .find(|reading| reading.target == pick.target && reading.metric == pick.metric)?;
            Some(points(&pick.prediction, rule, reading))
        })
        .sum()
}

#[derive(Debug, thiserror::Error)]
#[error("entry {0} is not a UUIDv7")]
pub struct NotUuidV7(pub Uuid);

/// Total score: `max(1, base) * 10_000 - (entry creation millis % 10_000)`.
///
/// Higher base scores dominate; for equal scores the earlier UUIDv7 entry
/// ranks higher. Four timestamp digits keep the outcome space small while
/// making collisions negligible for up to 10,000 entries a day. Remaining
/// ties rank by entry id.
pub fn total_score(entry_id: Uuid, base_score: u64) -> Result<i64, NotUuidV7> {
    let (seconds, nanos) = entry_id
        .get_timestamp()
        .ok_or(NotUuidV7(entry_id))?
        .to_unix();
    let millis = seconds * 1000 + u64::from(nanos) / 1_000_000;
    let total = base_score.clamp(1, u64::MAX / 10_000) * 10_000 - millis % 10_000;
    Ok(i64::try_from(total).unwrap_or(i64::MAX))
}

/// A scored entry, identified by its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scored {
    pub id: Uuid,
    pub base_score: u64,
    pub total_score: i64,
}

/// Indices (in id order) of the winning entries, best first. When nobody
/// scored, every entry wins: that is the "refund all" outcome.
pub fn winning_indices(entries: &[Scored], places: usize) -> Vec<usize> {
    let ordered: Vec<Scored> = entries
        .iter()
        .copied()
        .sorted_by_key(|entry| entry.id)
        .collect();
    if ordered.iter().all(|entry| entry.base_score == 0) {
        return (0..ordered.len()).collect();
    }
    (0..ordered.len())
        .sorted_by_key(|&index| Reverse(ordered[index].total_score))
        .take(places)
        .collect()
}

/// Number of announced outcomes: every ordered choice of `places` winners
/// from `entries`, plus the refund-all outcome. `None` on overflow.
pub fn outcome_count(entries: usize, places: usize) -> Option<usize> {
    if places > entries {
        return Some(1);
    }
    (entries - places + 1..=entries)
        .try_fold(1usize, |count, factor| count.checked_mul(factor))?
        .checked_add(1)
}

/// Every announced outcome, in the order the coordinator also generates.
pub fn ranking_outcomes(entries: usize, places: usize) -> Vec<Vec<usize>> {
    let mut outcomes: Vec<Vec<usize>> = (0..entries).permutations(places).collect();
    outcomes.push((0..entries).collect());
    outcomes
}

/// The attested message for an outcome: each winner index as a big-endian
/// `u64`. Fixed width so the message does not depend on the platform.
pub fn outcome_message(winners: &[usize]) -> Vec<u8> {
    winners
        .iter()
        .flat_map(|&index| (index as u64).to_be_bytes())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::{NoContext, Timestamp};

    fn reading(baseline: f64, observed: f64) -> Reading {
        Reading {
            target: "KORD".into(),
            metric: "m".into(),
            baseline: Some(baseline),
            observed: Some(observed),
        }
    }

    fn entry_id(millis: u64) -> Uuid {
        Uuid::new_v7(Timestamp::from_unix(
            NoContext,
            millis / 1000,
            (millis % 1000) as u32 * 1_000_000,
        ))
    }

    #[test]
    fn predictions_score_against_the_baseline_by_par_rule() {
        use ValueOptions::{Over, Par, Under};
        assert_eq!(
            points(&Par, ParRule::Exact, &reading(70.0, 70.0)),
            PAR_POINTS
        );
        assert_eq!(points(&Par, ParRule::Exact, &reading(70.0, 71.0)), 0);
        assert_eq!(
            points(&Over, ParRule::Exact, &reading(70.0, 71.0)),
            OVER_OR_UNDER_POINTS
        );
        assert_eq!(points(&Over, ParRule::Exact, &reading(70.0, 69.0)), 0);
        assert_eq!(
            points(&Under, ParRule::Exact, &reading(70.0, 69.0)),
            OVER_OR_UNDER_POINTS
        );
        assert_eq!(
            points(&Par, ParRule::Rounded, &reading(70.0, 70.4)),
            PAR_POINTS
        );
        assert_eq!(points(&Over, ParRule::Rounded, &reading(70.0, 70.4)), 0);
        assert_eq!(
            points(&Par, ParRule::Within(0.1), &reading(0.3, 0.35)),
            PAR_POINTS
        );
        assert_eq!(points(&Par, ParRule::Within(0.1), &reading(0.3, 0.5)), 0);
        assert_eq!(
            points(&Par, ParRule::Compass(22.0), &reading(350.0, 10.0)),
            PAR_POINTS
        );
        assert_eq!(
            points(&Par, ParRule::Compass(22.0), &reading(300.0, 10.0)),
            0
        );
        let missing = Reading {
            observed: None,
            ..reading(1.0, 1.0)
        };
        assert_eq!(points(&Par, ParRule::Exact, &missing), 0);
    }

    #[test]
    fn only_enabled_metrics_score() {
        let picks = vec![
            Pick {
                target: "KORD".into(),
                metric: "m".into(),
                prediction: ValueOptions::Par,
            },
            Pick {
                target: "KORD".into(),
                metric: "other".into(),
                prediction: ValueOptions::Par,
            },
        ];
        let metric = |id: &str| {
            Some(Metric {
                id: if id == "m" { "m" } else { "other" },
                par: ParRule::Exact,
            })
        };
        let readings = vec![reading(1.0, 1.0)];
        assert_eq!(
            base_score(&picks, &readings, &["m".into()], metric),
            PAR_POINTS
        );
        assert_eq!(base_score(&picks, &readings, &["other".into()], metric), 0);
    }

    #[test]
    fn earlier_entries_win_ties_and_scores_stay_positive() {
        let earlier = entry_id(1_000_001);
        let later = entry_id(1_000_002);
        assert!(total_score(earlier, 20).unwrap() > total_score(later, 20).unwrap());
        assert!(total_score(earlier, 0).unwrap() > 0);
        assert!(total_score(later, 20).unwrap() > total_score(earlier, 10).unwrap());
        assert!(total_score(Uuid::from_u128(0x1234), 20).is_err());
    }

    #[test]
    fn winners_follow_score_order_by_stable_entry_index() {
        let scored = |millis, base| Scored {
            id: entry_id(millis),
            base_score: base,
            total_score: total_score(entry_id(millis), base).unwrap(),
        };
        let (a, b, c) = (scored(1, 0), scored(2, 30), scored(3, 20));
        assert_eq!(winning_indices(&[c, a, b], 2), vec![1, 2]);
        assert_eq!(winning_indices(&[c, a, b], 5), vec![1, 2, 0]);
        assert_eq!(
            winning_indices(&[scored(1, 0), scored(2, 0)], 1),
            vec![0, 1]
        );
        assert!(winning_indices(&[], 1).is_empty());
    }

    #[test]
    fn outcome_encoding_is_stable() {
        let mut expected = 1u64.to_be_bytes().to_vec();
        expected.extend(2u64.to_be_bytes());
        assert_eq!(outcome_message(&[1, 2]), expected);
        assert_eq!(ranking_outcomes(3, 2).len(), 7);
        assert_eq!(ranking_outcomes(3, 2)[0], vec![0, 1]);
        assert_eq!(ranking_outcomes(3, 2)[6], vec![0, 1, 2]);
        assert_eq!(outcome_count(3, 2), Some(7));
        assert_eq!(outcome_count(25, 3), Some(13_801));
        assert_eq!(outcome_count(25, 5), Some(6_375_601));
        assert_eq!(outcome_count(usize::MAX, 3), None);
        for (entries, places) in [(5, 3), (20, 3), (10, 1)] {
            assert_eq!(
                ranking_outcomes(entries, places).len(),
                outcome_count(entries, places).unwrap()
            );
        }
    }
}
