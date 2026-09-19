//! Data sources the oracle attests to.
//!
//! The pipeline is the same for every source: a daemon publishes parquet
//! files, the oracle reads them, and at the signing date the oracle ranks
//! entries and attests the winning outcome. Only three things depend on the
//! source, and [`OutcomeSource`] captures them:
//!
//! 1. which targets an event may watch (weather stations, tide gauges, ...),
//! 2. which metrics can be predicted and what "par" means for each, and
//! 3. the baseline (forecast) and observed value of each metric per target
//!    over an event's observation window.
//!
//! Entries predict `Over`, `Par`, or `Under` the baseline for chosen
//! `(target, metric)` pairs, and [`crate::scoring`] turns readings into
//! scores the same way for every source.
//!
//! To add a source: implement [`OutcomeSource`] next to [`noaa`] and register
//! it in `startup.rs` where [`Sources::new`] is called. Events select it with
//! `"source": "<id>"` and entries use generic picks. Readings must be
//! deterministic for a given set of published files, because the
//! attestation is final. The attestation contract itself is in
//! `docs/attestation.md`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};
use time::OffsetDateTime;
use utoipa::ToSchema;

pub mod noaa;

pub use noaa::NoaaWeather;

/// Stable identifier stored with each event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(&'static str);

impl SourceId {
    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// How an observed value compares with the baseline for a `Par` prediction.
/// `Over` and `Under` always compare the raw values. Published by
/// `GET /oracle/sources` as `{"rule": "within", "tolerance": 0.1}`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, ToSchema)]
#[serde(tag = "rule", content = "tolerance", rename_all = "snake_case")]
pub enum ParRule {
    /// Par when the values are equal.
    Exact,
    /// Values are rounded to whole units before every comparison.
    Rounded,
    /// Par when `|observed - baseline| <= tolerance`.
    Within(f64),
    /// Par when the angular distance on a 360° compass is within the tolerance.
    Compass(f64),
}

/// A metric a source can score.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, ToSchema)]
pub struct Metric {
    /// Stable wire and storage name, e.g. `temp_high`.
    pub id: &'static str,
    pub par: ParRule,
}

/// The time range an event watches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservationWindow {
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
}

/// Baseline and observation for one metric at one target. `None` means the
/// source has no value; such readings never earn points.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Reading {
    pub target: String,
    /// A [`Metric::id`] of the source.
    pub metric: String,
    pub baseline: Option<f64>,
    pub observed: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("invalid target {target:?}: {reason}")]
    InvalidTarget { target: String, reason: String },
    #[error("source data is unavailable")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[async_trait]
pub trait OutcomeSource: Send + Sync {
    fn id(&self) -> SourceId;

    /// Every metric this source can score.
    fn metrics(&self) -> &'static [Metric];

    /// Metrics an event scores when it names none. All by default.
    fn default_metrics(&self) -> Vec<&'static str> {
        self.metrics().iter().map(|metric| metric.id).collect()
    }

    /// Rejects target ids the source cannot read. Called before a target is
    /// stored or used in a query.
    fn validate_target(&self, target: &str) -> Result<(), SourceError>;

    /// Readings for `targets` over `window`. Targets without data are
    /// omitted rather than reported as errors.
    async fn readings(
        &self,
        window: ObservationWindow,
        targets: &[String],
    ) -> Result<Vec<Reading>, SourceError>;

    fn metric(&self, id: &str) -> Option<Metric> {
        self.metrics()
            .iter()
            .copied()
            .find(|metric| metric.id == id)
    }
}

/// The registered sources. Events record which source they use; loading an
/// event for an unregistered source fails closed.
#[derive(Clone)]
pub struct Sources {
    sources: Vec<Arc<dyn OutcomeSource>>,
}

impl Sources {
    /// `default` serves events that name no source.
    pub fn new(
        default: Arc<dyn OutcomeSource>,
        others: impl IntoIterator<Item = Arc<dyn OutcomeSource>>,
    ) -> Self {
        Self {
            sources: std::iter::once(default).chain(others).collect(),
        }
    }

    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn OutcomeSource>> {
        self.sources.iter()
    }

    pub fn default_source(&self) -> &Arc<dyn OutcomeSource> {
        &self.sources[0]
    }

    pub fn get(&self, id: &str) -> Option<&Arc<dyn OutcomeSource>> {
        self.sources
            .iter()
            .find(|source| source.id().as_str() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn par_rules_have_a_stable_encoding() {
        let encode = |rule| serde_json::to_string(&rule).unwrap();
        assert_eq!(encode(ParRule::Exact), r#"{"rule":"exact"}"#);
        assert_eq!(encode(ParRule::Rounded), r#"{"rule":"rounded"}"#);
        assert_eq!(
            encode(ParRule::Within(0.1)),
            r#"{"rule":"within","tolerance":0.1}"#
        );
        assert_eq!(
            encode(ParRule::Compass(22.0)),
            r#"{"rule":"compass","tolerance":22.0}"#
        );
    }
}
