//! Bounded, overlapping history collection. A successful query certifies only
//! which interval was requested successfully, not the correctness of its values.
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration as StdDuration,
};

use anyhow::{Context, Result, anyhow, ensure};
use async_compression::tokio::bufread::GzipEncoder;
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::io::AsyncReadExt;

use super::download_observations::sha256_hex;
use crate::{CurrentWeather, Metar, ObservationData, XmlFetcher, parse_xml};

pub const HISTORY_SOURCE: &str = "https://aviationweather.gov/api/data/metar";
const RESPONSE_CAP: usize = 400;
const MAX_REQUESTS: usize = 512;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const COLLECTION_BUDGET: StdDuration = StdDuration::from_secs(20 * 60);
const REQUEST_SPACING: StdDuration = StdDuration::from_millis(750);

/// AWC filters `date` and `hours` to the minute, so a response can hold
/// reports from the minute around either end of the requested interval. Those
/// are dropped; a report further out means the source answered another query.
const SOURCE_MINUTE_SLACK: Duration = Duration::seconds(60);

#[derive(Clone, Debug)]
pub struct HistoryConfig {
    pub hours: u32,
    pub batch_size: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            hours: 4,
            batch_size: 25,
        }
    }
}

impl HistoryConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=24).contains(&self.hours),
            "observation_history_hours must be between 1 and 24"
        );
        ensure!(
            (1..=25).contains(&self.batch_size),
            "observation_batch_size must be between 1 and 25"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoverageBatch {
    pub station_ids: Vec<String>,
    pub window_start: String,
    pub window_end: String,
    pub requested_at: String,
    pub completed_at: String,
    pub status: String,
    pub source_url: String,
    pub source_sha256: Option<String>,
    pub response_status: Option<u16>,
    pub response_count: u64,
    pub report_count: u64,
    pub error: Option<String>,
    /// A bad received report requires review; transport failure only leaves a gap.
    #[serde(default)]
    pub failure_kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservationCoverage {
    pub version: String,
    pub interval: String,
    pub window_start: String,
    pub window_end: String,
    pub completed_at: String,
    pub batches: Vec<CoverageBatch>,
}

#[derive(Debug, Serialize)]
pub struct ArchivedResponse {
    pub source_url: String,
    pub response_status: u16,
    pub encoding: &'static str,
    pub body: String,
}

pub struct HistoryCollection {
    pub reports: Vec<Metar>,
    pub coverage: ObservationCoverage,
    pub sources: BTreeMap<String, ArchivedResponse>,
    pub precipitation: Option<super::shef::ShefCollection>,
}

impl HistoryCollection {
    /// A catalog failure has no station list and certifies no interval.
    pub fn unavailable(
        started_at: OffsetDateTime,
        config: &HistoryConfig,
        error: String,
    ) -> Result<Self> {
        config.validate()?;
        let end = started_at.replace_nanosecond(0)? - Duration::seconds(1);
        let start = end - Duration::hours(i64::from(config.hours));
        let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
        let mut batch = Query {
            stations: Vec::new(),
            start,
            end,
        }
        .receipt(started_at)?;
        batch.error = Some(error);
        batch.failure_kind = Some("transport".into());
        batch.completed_at = now.clone();
        Ok(Self {
            reports: Vec::new(),
            sources: BTreeMap::new(),
            precipitation: None,
            coverage: ObservationCoverage {
                version: "awc-history-v1".into(),
                interval: "closed".into(),
                window_start: start.format(&Rfc3339)?,
                window_end: end.format(&Rfc3339)?,
                completed_at: now,
                batches: vec![batch],
            },
        })
    }
}

#[async_trait]
pub(crate) trait HistoryFetcher: Send + Sync {
    async fn request(&self, url: &str) -> Result<(u16, String)>;
}

#[async_trait]
impl HistoryFetcher for XmlFetcher {
    async fn request(&self, url: &str) -> Result<(u16, String)> {
        self.fetch_history_xml(url).await.map_err(Into::into)
    }
}

#[derive(Clone)]
struct Query {
    stations: Vec<String>,
    start: OffsetDateTime,
    end: OffsetDateTime,
}

impl Query {
    fn url(&self) -> Result<String> {
        // AWC documents hours/date, but not endpoint inclusivity. Query one
        // extra second on each side and claim only the inner closed interval.
        let date = (self.end + Duration::seconds(1)).format(&Rfc3339)?;
        let hours = ((self.end - self.start).as_seconds_f64() + 2.0) / 3600.0;
        let mut url = reqwest::Url::parse(HISTORY_SOURCE)?;
        url.query_pairs_mut()
            .append_pair("ids", &self.stations.join(","))
            .append_pair("format", "xml")
            .append_pair("date", &date)
            .append_pair("hours", &format!("{hours:.8}"));
        Ok(url.to_string())
    }

    fn split(&self) -> Option<(Self, Self)> {
        if self.stations.len() > 1 {
            let middle = self.stations.len() / 2;
            return Some((
                Self {
                    stations: self.stations[..middle].to_vec(),
                    ..self.clone()
                },
                Self {
                    stations: self.stations[middle..].to_vec(),
                    ..self.clone()
                },
            ));
        }
        let seconds = (self.end - self.start).whole_seconds();
        if seconds <= 60 {
            return None;
        }
        let middle = self.start + Duration::seconds(seconds / 2);
        Some((
            Self {
                end: middle,
                ..self.clone()
            },
            Self {
                start: middle,
                ..self.clone()
            },
        ))
    }

    fn receipt(&self, requested_at: OffsetDateTime) -> Result<CoverageBatch> {
        Ok(CoverageBatch {
            station_ids: self.stations.clone(),
            window_start: self.start.format(&Rfc3339)?,
            window_end: self.end.format(&Rfc3339)?,
            requested_at: requested_at.format(&Rfc3339)?,
            completed_at: requested_at.format(&Rfc3339)?,
            status: "failed".into(),
            source_url: self.url()?,
            source_sha256: None,
            response_status: None,
            response_count: 0,
            report_count: 0,
            error: None,
            failure_kind: None,
        })
    }
}

pub async fn collect_history(
    fetcher: Arc<XmlFetcher>,
    station_ids: Vec<String>,
    end: OffsetDateTime,
    config: &HistoryConfig,
) -> Result<HistoryCollection> {
    collect(
        fetcher.as_ref(),
        station_ids,
        end,
        config,
        REQUEST_SPACING,
        COLLECTION_BUDGET,
        MAX_REQUESTS,
    )
    .await
}

async fn collect(
    fetcher: &dyn HistoryFetcher,
    mut station_ids: Vec<String>,
    run_started_at: OffsetDateTime,
    config: &HistoryConfig,
    spacing: StdDuration,
    budget: StdDuration,
    max_requests: usize,
) -> Result<HistoryCollection> {
    config.validate()?;
    ensure!(
        !station_ids.is_empty(),
        "station catalog is empty; cannot certify an empty weather source"
    );
    station_ids.sort();
    station_ids.dedup();
    ensure!(
        station_ids.iter().all(|station| !station.is_empty()
            && station.bytes().all(|byte| byte.is_ascii_alphanumeric())),
        "station catalog contains an invalid identifier"
    );
    let end = run_started_at.replace_nanosecond(0)? - Duration::seconds(1);
    let start = end - Duration::hours(i64::from(config.hours));
    let mut pending: VecDeque<Query> = station_ids
        .chunks(config.batch_size)
        .map(|stations| Query {
            stations: stations.to_vec(),
            start,
            end,
        })
        .collect();
    let mut collection = HistoryCollection {
        reports: Vec::new(),
        sources: BTreeMap::new(),
        precipitation: None,
        coverage: ObservationCoverage {
            version: "awc-history-v1".into(),
            interval: "closed".into(),
            window_start: start.format(&Rfc3339)?,
            window_end: end.format(&Rfc3339)?,
            completed_at: run_started_at.format(&Rfc3339)?,
            batches: Vec::new(),
        },
    };
    let deadline = tokio::time::Instant::now() + budget;
    let mut next_request = tokio::time::Instant::now();
    let mut attempts = 0;
    let mut source_bytes = 0;
    while let Some(query) = pending.pop_front() {
        let mut receipt = query.receipt(OffsetDateTime::now_utc())?;
        if attempts >= max_requests || tokio::time::Instant::now() >= deadline {
            receipt.error = Some("history collection request/time budget exhausted".into());
            receipt.failure_kind = Some("budget".into());
            collection.coverage.batches.push(receipt);
            continue;
        }
        tokio::time::sleep_until(next_request).await;
        receipt.requested_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
        attempts += 1;
        next_request = tokio::time::Instant::now() + spacing;
        let response =
            tokio::time::timeout_at(deadline, fetcher.request(&receipt.source_url)).await;
        receipt.completed_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
        let (status, body) = match response {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                receipt.error = Some(format!("history request failed: {error:#}"));
                receipt.failure_kind = Some("transport".into());
                collection.coverage.batches.push(receipt);
                continue;
            }
            Err(_) => {
                receipt.error = Some("history request exceeded collection deadline".into());
                receipt.failure_kind = Some("transport".into());
                collection.coverage.batches.push(receipt);
                continue;
            }
        };
        receipt.response_status = Some(status);
        let hash = sha256_hex(body.as_bytes());
        receipt.source_sha256 = Some(hash.clone());
        source_bytes += body.len();
        if source_bytes > MAX_SOURCE_BYTES {
            receipt.error = Some("history raw-evidence size budget exhausted".into());
            receipt.failure_kind = Some("budget".into());
            collection.coverage.batches.push(receipt);
            // No later response may restart a partially exhausted evidence budget.
            attempts = max_requests;
            continue;
        }
        let mut compressed = Vec::new();
        GzipEncoder::new(body.as_bytes())
            .read_to_end(&mut compressed)
            .await?;
        collection.sources.entry(hash).or_insert(ArchivedResponse {
            source_url: receipt.source_url.clone(),
            response_status: status,
            encoding: "gzip+base64",
            body: STANDARD.encode(compressed),
        });
        let parsed = parse_response(status, &body, &query);
        let (reports, response_count) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                receipt.error = Some(format!("history response rejected: {error:#}"));
                receipt.failure_kind = Some(
                    if matches!(status, 200 | 204) {
                        "response"
                    } else {
                        "transport"
                    }
                    .into(),
                );
                collection.coverage.batches.push(receipt);
                continue;
            }
        };
        receipt.response_count = response_count as u64;
        if response_count >= RESPONSE_CAP {
            receipt.failure_kind = Some("cap".into());
            receipt.error =
                Some("API result cap reached; parent query does not certify completeness".into());
            if let Some((left, right)) = query.split() {
                pending.push_front(right);
                pending.push_front(left);
            } else {
                receipt.error = Some("API result cap reached in minimum query interval".into());
            }
            collection.coverage.batches.push(receipt);
            continue;
        }
        let mut representation_failure = None;
        for report in &reports {
            if let Err(error) = CurrentWeather::try_from(report.clone()) {
                representation_failure = Some(format!(
                    "report {} cannot be represented: {error:#}",
                    report.station_id
                ));
                break;
            }
        }
        receipt.report_count = reports.len() as u64;
        if let Some(error) = representation_failure {
            receipt.error = Some(error);
            receipt.failure_kind = Some("representation".into());
        } else {
            receipt.status = if response_count == 0 {
                "empty"
            } else {
                "complete"
            }
            .into();
        }
        collection.reports.extend(reports);
        collection.coverage.batches.push(receipt);
    }
    collection.coverage.completed_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
    Ok(collection)
}

fn parse_response(status: u16, body: &str, query: &Query) -> Result<(Vec<Metar>, usize)> {
    if status == 204 {
        ensure!(
            body.trim().is_empty(),
            "204 response unexpectedly contains a body"
        );
        return Ok((Vec::new(), 0));
    }
    ensure!(status == 200, "unexpected history status {status}");
    let data: ObservationData = parse_xml(body).context("invalid history XML")?;
    ensure!(
        data.errors.trim().is_empty() && data.warnings.trim().is_empty(),
        "source returned errors or warnings"
    );
    ensure!(
        data.data_source.name == "metar",
        "response data source is not metar"
    );
    let declared: usize = data
        .data
        .num_results
        .as_deref()
        .ok_or_else(|| anyhow!("response lacks num_results"))?
        .parse()?;
    ensure!(
        declared == data.data.metar.len(),
        "response num_results does not match report count"
    );
    let mut reports = Vec::new();
    for report in data.data.metar {
        ensure!(
            query.stations.contains(&report.station_id),
            "unexpected station {} in response",
            report.station_id
        );
        let timestamp = OffsetDateTime::parse(
            report
                .observation_time
                .as_deref()
                .ok_or_else(|| anyhow!("report has no timestamp"))?,
            &Rfc3339,
        )?;
        ensure!(
            timestamp >= query.start - SOURCE_MINUTE_SLACK
                && timestamp <= query.end + SOURCE_MINUTE_SLACK,
            "source returned a report outside the requested interval"
        );
        if timestamp >= query.start && timestamp <= query.end {
            reports.push(report);
        }
    }
    Ok((reports, declared))
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
