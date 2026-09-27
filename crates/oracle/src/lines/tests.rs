use super::*;
use crate::sources::{Metric, ParRule, Reading, SourceId};
use async_trait::async_trait;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use time::macros::datetime;
use tokio_util::sync::CancellationToken;

fn band(lower: f64, upper: f64) -> Option<Band> {
    Some(Band { lower, upper })
}

/// Expected values come from `fit_cuts` in the airport audit's
/// `scripts/airport_audit/walk_forward.py`, run on the same inputs.
#[test]
fn fits_match_the_audit_script() {
    let a = [
        -3.2, -1.1, 0.4, 2.0, -0.5, 1.7, -2.2, 0.9, 3.3, -0.1, 0.0, 1.2,
    ];
    assert_eq!(fit(&a), band(-0.3, 1.05));
    let b: Vec<f64> = [
        0, 1, -1, 2, 0, 0, -2, 1, 3, -1, 0, 1, 2, -3, 0, 1, 0, -1, 2, 1,
    ]
    .map(f64::from)
    .to_vec();
    assert_eq!(fit(&b), band(-0.5, 0.5));
    let c: Vec<f64> = (0..200)
        .map(|k: i64| ((k * 7919) % 1000) as f64 / 100.0 - 5.0)
        .collect();
    assert_eq!(fit(&c), band(-1.715, 1.6149999999999998));
    let d: Vec<f64> = (0..90)
        .map(|k: i64| (((k * k * 31) % 17) - 8) as f64)
        .collect();
    assert_eq!(fit(&d), band(-2.5, 2.5));
}

#[test]
fn cuts_fall_between_misses_and_split_them_near_thirds() {
    let misses: Vec<f64> = (0..300).map(|k| f64::from(k % 7) - 3.0).collect();
    let fitted = fit(&misses).unwrap();
    for cut in [fitted.lower, fitted.upper] {
        assert_eq!(cut.fract().abs(), 0.5, "whole-number misses cut at halves");
    }
    let mut shares = [0usize; 3];
    for miss in &misses {
        shares[fitted.outcome(*miss) as usize] += 1;
    }
    for share in shares {
        assert!((share as f64 / 300.0 - 1.0 / 3.0).abs() < 0.1, "{shares:?}");
    }
}

#[test]
fn too_few_distinct_misses_have_no_line() {
    assert_eq!(fit(&[]), None);
    assert_eq!(fit(&[1.0, 1.0, 2.0, 2.0]), None);
    assert_eq!(fit(&[1.0, f64::NAN, 2.0, f64::INFINITY]), None);
    assert!(fit(&[1.0, 2.0, 3.0]).is_some());
}

#[test]
fn outcomes_include_both_cuts_in_par() {
    let line = Band {
        lower: -1.5,
        upper: 0.5,
    };
    assert_eq!(line.outcome(-1.6), ValueOptions::Under);
    assert_eq!(line.outcome(-1.5), ValueOptions::Par);
    assert_eq!(line.outcome(0.5), ValueOptions::Par);
    assert_eq!(line.outcome(0.6), ValueOptions::Over);
}

#[test]
fn events_use_the_nearest_fitted_length() {
    let settings = LineSettings {
        window_hours: vec![24, 72],
        ..LineSettings::default()
    };
    assert_eq!(settings.window_hours_for(Duration::hours(1)), Some(24));
    assert_eq!(settings.window_hours_for(Duration::hours(48)), Some(24));
    assert_eq!(settings.window_hours_for(Duration::hours(49)), Some(72));
    assert_eq!(settings.window_hours_for(Duration::days(5)), Some(72));
    let none = LineSettings {
        window_hours: vec![],
        ..LineSettings::default()
    };
    assert_eq!(none.window_hours_for(Duration::hours(24)), None);
}

#[test]
fn due_windows_are_utc_days_that_have_settled() {
    let settings = LineSettings {
        lookback_days: 3,
        ..LineSettings::default()
    };
    // 02:00 UTC: yesterday's window ended two hours ago, not yet settled.
    let now = datetime!(2030-01-10 02:00 UTC);
    assert_eq!(
        settings.due_windows(24, now),
        vec![
            datetime!(2030-01-08 00:00 UTC),
            datetime!(2030-01-07 00:00 UTC)
        ]
    );
    let later = datetime!(2030-01-10 03:00 UTC);
    assert_eq!(
        settings.due_windows(24, later),
        vec![
            datetime!(2030-01-09 00:00 UTC),
            datetime!(2030-01-08 00:00 UTC),
            datetime!(2030-01-07 00:00 UTC),
        ]
    );
}

fn pair(target: &str, metric: &str, day: i64, miss: f64) -> LinePair {
    LinePair {
        target: target.into(),
        metric: metric.into(),
        window_start: datetime!(2030-01-01 00:00 UTC) + Duration::days(day),
        baseline: 70.0,
        observed: 70.0 + miss,
    }
}

#[test]
fn targets_with_enough_windows_get_their_own_line_and_all_share_a_pooled_one() {
    let mut pairs = vec![];
    for day in 0..10 {
        pairs.push(pair("KAAA", "temp_high", day, (day % 5) as f64 - 2.0));
    }
    for day in 0..3 {
        pairs.push(pair("KBBB", "temp_high", day, day as f64 + 5.0));
    }
    let fitted_at = datetime!(2030-02-01 00:00 UTC);
    let lines = fit_lines(&pairs, 5, 24, fitted_at);
    assert_eq!(lines.len(), 2, "{lines:?}");
    let own = &lines[0];
    assert_eq!(
        (own.target.as_str(), own.level),
        ("KAAA", LineLevel::Station)
    );
    assert_eq!(own.windows, 10);
    assert_eq!(own.over + own.par + own.under, 10);
    assert_eq!(own.first_window, datetime!(2030-01-01 00:00 UTC));
    assert_eq!(own.last_window, datetime!(2030-01-10 00:00 UTC));
    let pooled = &lines[1];
    assert_eq!(
        (pooled.target.as_str(), pooled.level),
        ("", LineLevel::Pooled)
    );
    assert_eq!(pooled.windows, 13);

    let targets = ["KAAA".to_owned(), "KBBB".to_owned()];
    let resolved = resolve(&lines, &targets, &["temp_high".to_owned()]).unwrap();
    assert_eq!(resolved[0], lines[0]);
    assert_eq!(resolved[1].target, "KBBB");
    assert_eq!(resolved[1].level, LineLevel::Pooled);
    assert_eq!(resolved[1].band(), lines[1].band());
    assert_eq!(
        resolve(&lines, &targets, &["wind_speed".to_owned()]),
        Err(vec![
            "KAAA/wind_speed".to_owned(),
            "KBBB/wind_speed".to_owned()
        ])
    );
}

/// Three stations; KCCC never reports, so it has no pairs of its own.
/// Misses vary by day and station. Days in `empty` have no observations.
struct History {
    reads: Mutex<Vec<(OffsetDateTime, usize)>>,
    empty: Mutex<HashSet<OffsetDateTime>>,
}

impl History {
    fn new() -> Self {
        Self {
            reads: Mutex::new(vec![]),
            empty: Mutex::new(HashSet::new()),
        }
    }
}

#[async_trait]
impl OutcomeSource for History {
    fn id(&self) -> SourceId {
        SourceId::new("history")
    }

    fn metrics(&self) -> &'static [Metric] {
        &[
            Metric {
                id: "m",
                par: ParRule::Exact,
                calibrated: true,
            },
            Metric {
                id: "n",
                par: ParRule::Exact,
                calibrated: false,
            },
        ]
    }

    fn validate_target(&self, _target: &str) -> Result<(), SourceError> {
        Ok(())
    }

    async fn line_targets(&self) -> Result<Vec<String>, SourceError> {
        Ok(vec!["KAAA".into(), "KBBB".into(), "KCCC".into()])
    }

    async fn readings(
        &self,
        window: ObservationWindow,
        targets: &[String],
    ) -> Result<Vec<Reading>, SourceError> {
        self.reads
            .lock()
            .unwrap()
            .push((window.start, targets.len()));
        let empty = self.empty.lock().unwrap().contains(&window.start);
        let day = window.start.unix_timestamp() / 86_400;
        Ok(targets
            .iter()
            .enumerate()
            .flat_map(|(index, target)| {
                let observed = (!empty && target != "KCCC")
                    .then(|| 70.0 + ((day * 7 + index as i64 * 3) % 9) as f64 - 4.0);
                ["m", "n"].map(|metric| Reading {
                    target: target.clone(),
                    metric: metric.into(),
                    baseline: Some(70.0),
                    observed,
                })
            })
            .collect())
    }
}

#[tokio::test]
async fn passes_fill_history_newest_first_then_fit_and_prune() {
    let directory = tempfile::tempdir().unwrap();
    let (db, writer) = Database::open(directory.path()).await.unwrap();
    let shutdown = CancellationToken::new();
    let writer = tokio::spawn(writer.run(shutdown.clone()));
    let source = Arc::new(History::new());
    let settings = LineSettings {
        lookback_days: 30,
        min_windows: 10,
        window_hours: vec![24],
        settle_after: Duration::hours(3),
        windows_per_pass: 5,
        targets_per_query: 2,
    };
    let now = datetime!(2030-01-31 12:00 UTC);
    let latest = datetime!(2030-01-30 00:00 UTC);
    source.empty.lock().unwrap().insert(latest);

    let first = run_pass(&db, source.as_ref(), &settings, now)
        .await
        .unwrap();
    assert_eq!(first.windows, 5);
    // Four windows with pairs from two stations; the latest was empty.
    assert_eq!(first.pairs, 8);
    assert_eq!(first.lines, 0, "no line has ten windows yet");
    {
        let reads = source.reads.lock().unwrap();
        assert_eq!(reads[0], (latest, 2), "newest first, two targets a query");
        assert_eq!(reads[1], (latest, 1));
        assert_eq!(reads.len(), 10);
    }

    let mut passes = 1;
    loop {
        let pass = run_pass(&db, source.as_ref(), &settings, now)
            .await
            .unwrap();
        if pass.windows == 0 {
            assert_eq!(pass.lines, 0, "no new windows and today's fit exists");
            break;
        }
        passes += 1;
    }
    assert_eq!(
        passes, 6,
        "30 windows, five a pass; the empty one waits a day"
    );
    let targets = ["KAAA".to_owned(), "KCCC".to_owned()];
    let window = ObservationWindow {
        start: datetime!(2030-02-01 00:00 UTC),
        end: datetime!(2030-02-02 00:00 UTC),
    };
    let lines = current(
        &db,
        &settings,
        "history",
        window,
        &targets,
        &["m".to_owned()],
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(lines[0].level, LineLevel::Station);
    assert_eq!(lines[0].windows, 29);
    assert_eq!(lines[1].target, "KCCC");
    assert_eq!(lines[1].level, LineLevel::Pooled);
    assert_eq!(lines[1].windows, 58);
    assert_eq!(
        current(
            &db,
            &settings,
            "history",
            window,
            &targets,
            &["n".to_owned()]
        )
        .await
        .unwrap(),
        Err(vec!["KAAA/n".to_owned(), "KCCC/n".to_owned()]),
        "uncalibrated metrics are never read"
    );

    // A day after it ended, the empty window is read once more.
    source.empty.lock().unwrap().clear();
    let next_day = datetime!(2030-02-01 01:00 UTC);
    let reread = run_pass(&db, source.as_ref(), &settings, next_day)
        .await
        .unwrap();
    assert_eq!((reread.windows, reread.pairs), (1, 2));
    assert_eq!(
        run_pass(&db, source.as_ref(), &settings, next_day)
            .await
            .unwrap()
            .windows,
        0
    );

    // Six weeks on, the January history has aged out of the lookback.
    let later = datetime!(2030-03-15 12:00 UTC);
    run_pass(&db, source.as_ref(), &settings, later)
        .await
        .unwrap();
    let kept = db
        .line_pairs("history", 24, "m", datetime!(2000-01-01 00:00 UTC))
        .await
        .unwrap();
    assert!(
        kept.iter()
            .all(|pair| pair.window_start >= datetime!(2030-02-06 00:00 UTC)),
        "history before the lookback and a week's margin is pruned"
    );
    let fitted = db
        .line_fits("history", 24, &["KAAA".to_owned()], &["m".to_owned()])
        .await
        .unwrap();
    assert!(
        fitted
            .iter()
            .all(|line| line.fitted_at == later && line.first_window >= settings.since(later)),
        "{fitted:?}"
    );

    shutdown.cancel();
    writer.await.unwrap().unwrap();
}
