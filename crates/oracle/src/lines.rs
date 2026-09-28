//! Self-calibrating Par lines: the `lines` scoring rules.
//!
//! Under the fixed rules Par means the observation matches the forecast to
//! the whole unit. That happens far less than a third of the time, and each
//! station's forecast leans one way, so a player who repeats the pick that
//! did best before wins most games.
//!
//! A line is a band on the miss `observed - baseline` for one target and
//! metric. `Under` when the miss is below `lower`, `Over` when it is above
//! `upper`, `Par` otherwise. Exactly one outcome happens, and every right
//! pick earns [`crate::scoring::LINE_POINTS`]. The band is fitted so that
//! over the trailing [`LineSettings::lookback_days`] each outcome happened
//! about a third of the time ([`fit`]):
//!
//! - Cuts sit midway between neighbouring past misses, never on one, so
//!   whole-number misses (wind knots, humidity percent) cannot pile onto Par.
//! - A target with fewer than [`LineSettings::min_windows`] past windows uses
//!   the pooled line of every target the source tracks.
//!
//! History is read with the same provisional reader the oracle shows while a
//! window runs, for windows that start at 00:00 UTC. Each pass reads a few
//! windows that have ended, newest first, and refits when it read any, or
//! once a UTC day, so lines follow the forecasts as they drift. An event
//! copies the current lines when it is created, or an earlier event's copy
//! ([`copy_frozen`]), and is scored against that copy, so a line never
//! changes under an event.

use std::collections::BTreeMap;

use log::info;
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, Time};
use utoipa::ToSchema;

use crate::{
    database::{Database, WriteError},
    events::ValueOptions,
    sources::{ObservationWindow, OutcomeSource, SourceError},
};

/// History older than the lookback is kept this much longer, then pruned.
const PRUNE_AFTER_LOOKBACK: Duration = Duration::days(7);

/// Where Par sits on the miss `observed - baseline`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Band {
    pub lower: f64,
    pub upper: f64,
}

impl Band {
    /// The outcome of a miss. Both cuts belong to Par.
    pub fn outcome(self, miss: f64) -> ValueOptions {
        if miss < self.lower {
            ValueOptions::Under
        } else if miss > self.upper {
            ValueOptions::Over
        } else {
            ValueOptions::Par
        }
    }
}

/// Cuts between past misses that come closest to three equal outcomes, or
/// `None` with fewer than three distinct finite misses.
///
/// Cut `i` sits midway between the `i`-th and next distinct miss. The pair
/// kept is the one whose largest share is nearest a third, trying the three
/// cuts either side of the 1/3 and 2/3 marks; on equal shares the lower cuts
/// win. This is `fit_cuts` in the airport audit's `walk_forward.py`, step
/// for step, so both give the same lines.
pub fn fit(misses: &[f64]) -> Option<Band> {
    const NEIGHBOURS: usize = 3;
    let mut values: Vec<f64> = misses
        .iter()
        .copied()
        .filter(|miss| miss.is_finite())
        .collect();
    values.sort_by(f64::total_cmp);
    let mut distinct = values.clone();
    distinct.dedup();
    if distinct.len() < 3 {
        return None;
    }
    let n = values.len() as f64;
    let cuts: Vec<f64> = distinct
        .windows(2)
        .map(|pair| (pair[0] + pair[1]) / 2.0)
        .collect();
    // below[i]: misses at or under distinct[i], so under cut i.
    let below: Vec<usize> = distinct[..distinct.len() - 1]
        .iter()
        .map(|low| values.partition_point(|value| value <= low))
        .collect();
    let around = |target: f64| {
        let i = below.partition_point(|&count| (count as f64) < target * n);
        i.saturating_sub(NEIGHBOURS)..(i + NEIGHBOURS).min(cuts.len())
    };
    let third = 1.0 / 3.0;
    let mut best: Option<(f64, usize, usize)> = None;
    for a in around(third) {
        for b in around(2.0 / 3.0) {
            if b <= a {
                continue;
            }
            let shares = [below[a], below[b] - below[a], values.len() - below[b]];
            let worst = shares
                .iter()
                .map(|&share| (share as f64 / n - third).abs())
                .fold(0.0, f64::max);
            if best.is_none_or(|(least, _, _)| worst < least) {
                best = Some((worst, a, b));
            }
        }
    }
    best.map(|(_, a, b)| Band {
        lower: cuts[a],
        upper: cuts[b],
    })
}

/// Whose history a line was fitted on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineLevel {
    /// The target's own windows.
    Station,
    /// Every tracked target's windows, for a target with too few of its own.
    Pooled,
}

impl LineLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Station => "station",
            Self::Pooled => "pooled",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        [Self::Station, Self::Pooled]
            .into_iter()
            .find(|level| level.as_str() == value)
    }
}

/// A fitted line and the history behind it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Line {
    pub target: String,
    pub metric: String,
    /// `Under` below this miss (`observed - baseline`), in the metric's unit
    pub lower: f64,
    /// `Over` above this miss; `Par` from `lower` to `upper`, both included
    pub upper: f64,
    pub level: LineLevel,
    /// Length of the past windows the line was fitted on
    pub window_hours: i64,
    /// Past windows the line was fitted on, and how many of them fell
    /// `Over`, `Par`, and `Under` it
    pub windows: i64,
    pub over: i64,
    pub par: i64,
    pub under: i64,
    /// Start of the earliest and latest past window used
    #[serde(with = "time::serde::rfc3339")]
    pub first_window: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_window: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub fitted_at: OffsetDateTime,
}

impl Line {
    pub fn band(&self) -> Band {
        Band {
            lower: self.lower,
            upper: self.upper,
        }
    }
}

/// One past window's forecast and observation for a target and metric.
#[derive(Clone, Debug, PartialEq)]
pub struct LinePair {
    pub target: String,
    pub metric: String,
    pub window_start: OffsetDateTime,
    pub baseline: f64,
    pub observed: f64,
}

/// How lines are fitted and their history read.
#[derive(Clone, Debug)]
pub struct LineSettings {
    /// Days of past windows a fit uses.
    pub lookback_days: i64,
    /// Past windows a target needs for a line of its own.
    pub min_windows: usize,
    /// Window lengths lines are fitted for. An event uses the nearest.
    pub window_hours: Vec<i64>,
    /// How long after a window ends its history is read, so late reports
    /// are in.
    pub settle_after: Duration,
    /// Windows read per pass, so a pass stays short while history fills.
    pub windows_per_pass: usize,
    /// Targets per source query.
    pub targets_per_query: usize,
}

impl Default for LineSettings {
    fn default() -> Self {
        Self {
            lookback_days: 60,
            min_windows: 20,
            window_hours: vec![24],
            settle_after: Duration::hours(3),
            windows_per_pass: 6,
            targets_per_query: 100,
        }
    }
}

impl LineSettings {
    /// The fitted window length for an event window of `length`.
    pub fn window_hours_for(&self, length: Duration) -> Option<i64> {
        self.window_hours
            .iter()
            .copied()
            .min_by_key(|&hours| (Duration::hours(hours) - length).abs())
    }

    /// Start of the first past window a fit on `now`'s UTC day uses.
    pub fn since(&self, now: OffsetDateTime) -> OffsetDateTime {
        utc_midnight(now) - Duration::days(self.lookback_days)
    }

    /// Starts of the `hours` windows inside the lookback that ended at least
    /// [`Self::settle_after`] ago, newest first.
    fn due_windows(&self, hours: i64, now: OffsetDateTime) -> Vec<OffsetDateTime> {
        let since = self.since(now);
        let mut start = utc_midnight(now - self.settle_after - Duration::hours(hours));
        let mut starts = vec![];
        while start >= since {
            starts.push(start);
            start -= Duration::DAY;
        }
        starts
    }
}

pub fn utc_midnight(time: OffsetDateTime) -> OffsetDateTime {
    time.to_offset(time::UtcOffset::UTC)
        .replace_time(Time::MIDNIGHT)
}

/// Lines for every target with at least `min_windows` pairs of a metric, and
/// a pooled line per metric (empty `target`) from all of them.
pub fn fit_lines(
    pairs: &[LinePair],
    min_windows: usize,
    window_hours: i64,
    fitted_at: OffsetDateTime,
) -> Vec<Line> {
    let mut stations: BTreeMap<(&str, &str), Vec<&LinePair>> = BTreeMap::new();
    let mut pooled: BTreeMap<&str, Vec<&LinePair>> = BTreeMap::new();
    for pair in pairs {
        stations
            .entry((pair.metric.as_str(), pair.target.as_str()))
            .or_default()
            .push(pair);
        pooled.entry(pair.metric.as_str()).or_default().push(pair);
    }
    let line = |target: &str, metric: &str, level, pairs: &[&LinePair]| {
        if pairs.len() < min_windows.max(1) {
            return None;
        }
        let misses: Vec<f64> = pairs
            .iter()
            .map(|pair| pair.observed - pair.baseline)
            .collect();
        let band = fit(&misses)?;
        let mut counts = [0i64; 3];
        for miss in &misses {
            counts[match band.outcome(*miss) {
                ValueOptions::Over => 0,
                ValueOptions::Par => 1,
                ValueOptions::Under => 2,
            }] += 1;
        }
        Some(Line {
            target: target.to_owned(),
            metric: metric.to_owned(),
            lower: band.lower,
            upper: band.upper,
            level,
            window_hours,
            windows: misses.len() as i64,
            over: counts[0],
            par: counts[1],
            under: counts[2],
            first_window: pairs.iter().map(|pair| pair.window_start).min()?,
            last_window: pairs.iter().map(|pair| pair.window_start).max()?,
            fitted_at,
        })
    };
    let own = stations
        .iter()
        .filter_map(|((metric, target), pairs)| line(target, metric, LineLevel::Station, pairs));
    let shared = pooled
        .iter()
        .filter_map(|(metric, pairs)| line("", metric, LineLevel::Pooled, pairs));
    own.chain(shared).collect()
}

/// The line each target and metric would score against now: its own, or
/// the pooled line for the metric. `fits` holds both kinds, pooled lines
/// with an empty target. Errors with the pairs that have neither.
pub fn resolve(
    fits: &[Line],
    targets: &[String],
    metrics: &[String],
) -> Result<Vec<Line>, Vec<String>> {
    let mut lines = vec![];
    let mut missing = vec![];
    for target in targets {
        for metric in metrics {
            let own = fits
                .iter()
                .find(|line| &line.target == target && &line.metric == metric);
            let pooled = || {
                fits.iter().find(|line| {
                    line.level == LineLevel::Pooled
                        && line.target.is_empty()
                        && &line.metric == metric
                })
            };
            match own.or_else(pooled) {
                Some(line) => lines.push(Line {
                    target: target.clone(),
                    ..line.clone()
                }),
                None => missing.push(format!("{target}/{metric}")),
            }
        }
    }
    if missing.is_empty() {
        Ok(lines)
    } else {
        Err(missing)
    }
}

/// Copies of the lines an earlier event froze, `frozen`, for `targets` and
/// `metrics`: for each pair, the frozen line fitted on `window_hours`
/// windows, exactly as stored. Errors with the pairs that have none; with no
/// `window_hours`, that is every pair.
pub fn copy_frozen(
    frozen: &[Line],
    window_hours: Option<i64>,
    targets: &[String],
    metrics: &[String],
) -> Result<Vec<Line>, Vec<String>> {
    let mut lines = vec![];
    let mut missing = vec![];
    for target in targets {
        for metric in metrics {
            match frozen.iter().find(|line| {
                &line.target == target
                    && &line.metric == metric
                    && Some(line.window_hours) == window_hours
            }) {
                Some(line) => lines.push(line.clone()),
                None => missing.push(format!("{target}/{metric}")),
            }
        }
    }
    if missing.is_empty() {
        Ok(lines)
    } else {
        Err(missing)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot read line history: {0}")]
    Source(#[from] SourceError),
    #[error("cannot read stored line history")]
    Read(#[from] sqlx::Error),
    #[error(transparent)]
    Write(#[from] WriteError),
}

/// What one pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinePass {
    /// Past windows read.
    pub windows: usize,
    /// Forecast and observation pairs stored from them.
    pub pairs: usize,
    /// Lines stored by refits.
    pub lines: usize,
}

/// Reads up to [`LineSettings::windows_per_pass`] past windows of history
/// for `source`, refits its lines when it read any or the last fit is from
/// an earlier UTC day, and prunes history past the lookback. Stops at the
/// first failed read; the next pass retries the window.
pub async fn run_pass(
    db: &Database,
    source: &dyn OutcomeSource,
    settings: &LineSettings,
    now: OffsetDateTime,
) -> Result<LinePass, Error> {
    let metrics: Vec<&str> = source
        .metrics()
        .iter()
        .filter(|metric| metric.calibrated)
        .map(|metric| metric.id)
        .collect();
    let mut pass = LinePass::default();
    if metrics.is_empty() {
        return Ok(pass);
    }
    let source_id = source.id().as_str();
    let since = settings.since(now);
    let mut budget = settings.windows_per_pass;
    let mut targets: Option<Vec<String>> = None;
    for &hours in &settings.window_hours {
        let read = db.line_windows(source_id, hours, since).await?;
        let due: Vec<OffsetDateTime> = settings
            .due_windows(hours, now)
            .into_iter()
            .filter(|start| {
                let late = *start + Duration::hours(hours) + Duration::DAY;
                match read.get(start) {
                    None => true,
                    // An empty read soon after the window may have come
                    // before its data did; read it once more a day later.
                    Some(&(0, read_at)) => read_at < late && now >= late,
                    Some(_) => false,
                }
            })
            .take(budget)
            .collect();
        let mut read_now = 0;
        for start in due {
            let targets = match &mut targets {
                Some(targets) => targets,
                None => targets.insert(source.line_targets().await?),
            };
            let window = ObservationWindow {
                start,
                end: start + Duration::hours(hours),
            };
            let mut pairs = vec![];
            for chunk in targets.chunks(settings.targets_per_query.max(1)) {
                for reading in source.line_readings(window, chunk).await? {
                    if !metrics.contains(&reading.metric.as_str()) {
                        continue;
                    }
                    if let (Some(baseline), Some(observed)) = (reading.baseline, reading.observed)
                        && baseline.is_finite()
                        && observed.is_finite()
                    {
                        pairs.push(LinePair {
                            target: reading.target,
                            metric: reading.metric,
                            window_start: start,
                            baseline,
                            observed,
                        });
                    }
                }
            }
            pass.pairs += pairs.len();
            db.store_line_window(source_id, hours, start, pairs, now)
                .await?;
            pass.windows += 1;
            read_now += 1;
            budget -= 1;
        }
        let stale = db
            .line_fitted_at(source_id, hours)
            .await?
            .is_none_or(|fitted| utc_midnight(fitted) < utc_midnight(now));
        if read_now > 0 || stale {
            let mut lines = vec![];
            for metric in &metrics {
                let pairs = db.line_pairs(source_id, hours, metric, since).await?;
                lines.extend(fit_lines(&pairs, settings.min_windows, hours, now));
            }
            pass.lines += lines.len();
            db.replace_line_fits(source_id, hours, lines).await?;
        }
        db.prune_line_history(source_id, hours, since - PRUNE_AFTER_LOOKBACK)
            .await?;
    }
    if pass.windows > 0 {
        info!(
            "lines for {source_id}: read {} past windows ({} pairs), stored {} lines",
            pass.windows, pass.pairs, pass.lines
        );
    }
    Ok(pass)
}

/// Lines an event on `source` over `window` would copy now, or the target
/// and metric pairs without one. Uses the fitted length nearest the window.
pub async fn current(
    db: &Database,
    settings: &LineSettings,
    source_id: &str,
    window: ObservationWindow,
    targets: &[String],
    metrics: &[String],
) -> Result<Result<Vec<Line>, Vec<String>>, sqlx::Error> {
    let Some(hours) = settings.window_hours_for(window.end - window.start) else {
        return Ok(Err(targets
            .iter()
            .flat_map(|target| {
                metrics
                    .iter()
                    .map(move |metric| format!("{target}/{metric}"))
            })
            .collect()));
    };
    let fits = db.line_fits(source_id, hours, targets, metrics).await?;
    Ok(resolve(&fits, targets, metrics))
}

#[cfg(test)]
mod tests;
