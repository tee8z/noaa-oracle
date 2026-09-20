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
    if !baseline.is_finite() || !observed.is_finite() {
        return 0;
    }
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

/// Stored score: `max(1, base) * 10_000`.
///
/// Equal point totals have equal displayed scores. Ranking separately uses
/// the complete UUIDv7 in ascending order, so timestamp boundaries cannot
/// let a later entry win a tie. The positive-zero convention is retained.
pub fn total_score(entry_id: Uuid, base_score: u64) -> Result<i64, NotUuidV7> {
    if entry_id.get_version_num() != 7 {
        return Err(NotUuidV7(entry_id));
    }
    let total = base_score.clamp(1, i64::MAX as u64 / 10_000) * 10_000;
    Ok(total as i64)
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
        .sorted_by_key(|&index| (Reverse(ordered[index].base_score), ordered[index].id))
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
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for prediction in [Over, Par, Under] {
                assert_eq!(
                    points(&prediction, ParRule::Exact, &reading(70.0, invalid)),
                    0
                );
                assert_eq!(
                    points(&prediction, ParRule::Exact, &reading(invalid, 70.0)),
                    0
                );
            }
        }
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
    fn equal_points_have_equal_stored_scores_and_zero_stays_positive() {
        let earlier = entry_id(1_000_001);
        let later = entry_id(1_000_002);
        assert_eq!(
            total_score(earlier, 20).unwrap(),
            total_score(later, 20).unwrap()
        );
        assert_eq!(total_score(earlier, 20).unwrap(), 200_000);
        assert!(total_score(earlier, 0).unwrap() > 0);
        assert!(total_score(later, 20).unwrap() > total_score(earlier, 10).unwrap());
        assert!(total_score(Uuid::from_u128(0x1234), 20).is_err());
    }

    #[test]
    fn earlier_entries_win_ties_across_timestamp_boundaries() {
        let scored = |millis| {
            let id = entry_id(millis);
            Scored {
                id,
                base_score: 20,
                total_score: total_score(id, 20).unwrap(),
            }
        };
        for (earlier, later) in [(9_999, 10_000), (86_399_999, 86_400_000)] {
            let first = scored(earlier);
            let second = scored(later);
            assert_eq!(first.total_score, second.total_score);
            assert_eq!(winning_indices(&[second, first], 1), vec![0]);
        }
    }

    #[test]
    fn ranking_uses_full_id_for_same_millisecond_ties_and_ignores_legacy_penalties() {
        let earlier_id = entry_id(9_999);
        let later_id = entry_id(10_000);
        let earlier = Scored {
            id: earlier_id,
            base_score: 20,
            total_score: 190_001,
        };
        let later = Scored {
            id: later_id,
            base_score: 20,
            total_score: 200_000,
        };
        assert_eq!(winning_indices(&[later, earlier], 1), vec![0]);

        let low_id = Uuid::from_u128(earlier_id.as_u128() & !0xffff);
        let high_id = Uuid::from_u128(low_id.as_u128() + 1);
        let same_time = [
            Scored {
                id: high_id,
                ..earlier
            },
            Scored {
                id: low_id,
                ..earlier
            },
        ];
        assert_eq!(winning_indices(&same_time, 1), vec![0]);
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
