//! Collection receipts prove which station/time ranges were actually requested.
//! An empty query or a recent filename alone cannot prove source coverage.

use super::{Error, sql_string_list};
use duckdb::Connection;
use serde::Deserialize;
use std::collections::BTreeMap;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone)]
pub(super) struct Requirement {
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    pub stations: Vec<String>,
    pub collected_after: OffsetDateTime,
    /// Metric groups the event's metrics depend on (see `quality_groups`).
    pub groups: Vec<&'static str>,
    /// The event's metrics; none means every metric.
    pub metrics: Vec<String>,
}

#[derive(Deserialize)]
struct Receipt {
    version: String,
    interval: String,
    batches: Vec<Batch>,
}

#[derive(Deserialize)]
struct Batch {
    station_ids: Vec<String>,
    window_start: String,
    window_end: String,
    requested_at: String,
    completed_at: String,
    status: String,
    source_url: String,
    source_sha256: Option<String>,
    response_status: Option<u16>,
    response_count: u64,
    report_count: u64,
    error: Option<String>,
    #[serde(default)]
    failure_kind: Option<String>,
}

impl Requirement {
    pub fn scores(&self, metric: &str) -> bool {
        self.metrics.is_empty() || self.metrics.iter().any(|scored| scored == metric)
    }

    pub fn unavailable(&self, reason: impl Into<String>) -> Error {
        Error::ObservationCoverage {
            stations: self.stations.clone(),
            reason: reason.into(),
        }
    }

    pub fn verify(&self, connection: &Connection, files: &[String]) -> Result<(), Error> {
        if files.is_empty() {
            return Err(self.unavailable("no collection receipts were retained for this window"));
        }
        let sql = format!(
            "SELECT decode(value) FROM parquet_kv_metadata([{}]) WHERE decode(key) = 'observation_coverage'",
            sql_string_list(files)
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let receipts = rows.collect::<Result<Vec<_>, _>>()?;
        self.verify_receipts(&receipts)
    }

    fn verify_receipts(&self, receipts: &[String]) -> Result<(), Error> {
        if self.stations.is_empty() || self.start > self.end {
            return Err(
                self.unavailable("settlement requires explicit stations and a bounded window")
            );
        }
        let mut spans = BTreeMap::<&str, Vec<(OffsetDateTime, OffsetDateTime)>>::new();
        let mut fresh = BTreeMap::<&str, bool>::new();
        let mut successes =
            BTreeMap::<&str, Vec<(OffsetDateTime, OffsetDateTime, OffsetDateTime)>>::new();
        let mut defects = Vec::new();
        let now = OffsetDateTime::now_utc();
        for json in receipts {
            let receipt: Receipt = serde_json::from_str(json)
                .map_err(|_| self.unavailable("a collection receipt is malformed"))?;
            if receipt.version != "awc-history-v1" || receipt.interval != "closed" {
                continue;
            }
            for batch in receipt.batches {
                if !matches!(batch.status.as_str(), "complete" | "empty") {
                    if matches!(
                        batch.failure_kind.as_deref(),
                        Some("response" | "representation")
                    ) {
                        defects.push(batch);
                    }
                    continue;
                }
                let parse = |value: &str| {
                    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
                        self.unavailable("a collection receipt has an invalid timestamp")
                    })
                };
                let start = parse(&batch.window_start)?;
                let end = parse(&batch.window_end)?;
                let requested = parse(&batch.requested_at)?;
                let completed = parse(&batch.completed_at)?;
                let trusted_url = batch
                    .source_url
                    .starts_with("https://aviationweather.gov/api/data/metar?");
                let hash = batch.source_sha256.as_deref().is_some_and(|hash| {
                    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                });
                let response = match (batch.status.as_str(), batch.response_status) {
                    ("complete", Some(200)) => hash && batch.response_count > 0,
                    ("empty", Some(200)) => hash && batch.response_count == 0,
                    ("empty", Some(204)) => batch.response_count == 0,
                    _ => false,
                };
                if !trusted_url
                    || !response
                    || batch.response_count >= 400
                    || batch.report_count > batch.response_count
                    || batch.error.is_some()
                    || start > end
                    || end > requested
                    || requested > completed
                    || completed > now
                {
                    continue;
                }
                for station in &self.stations {
                    if batch.station_ids.contains(station) {
                        spans.entry(station).or_default().push((start, end));
                        successes
                            .entry(station)
                            .or_default()
                            .push((start, end, requested));
                        if start <= self.end && end >= self.end && requested >= self.collected_after
                        {
                            fresh.insert(station, true);
                        }
                    }
                }
            }
        }
        for defect in defects {
            let parse = |value: &str| {
                OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
                    self.unavailable("a failed source receipt has an invalid timestamp")
                })
            };
            let start = parse(&defect.window_start)?.max(self.start);
            let end = parse(&defect.window_end)?.min(self.end);
            let completed = parse(&defect.completed_at)?;
            if start > end {
                continue;
            }
            for station in &self.stations {
                // Requests made after the failure, together, must cover what it missed.
                let later: Vec<_> = successes
                    .get(station.as_str())
                    .into_iter()
                    .flatten()
                    .filter(|(_, _, requested)| *requested >= completed)
                    .map(|(from, to, _)| (*from, *to))
                    .collect();
                if defect.station_ids.contains(station) && !covers(later, start, end) {
                    return Err(Error::ObservationCoverage {
                        stations: vec![station.clone()],
                        reason: "a received source response failed validation; later successful requests must cover the affected interval".into(),
                    });
                }
            }
        }
        for station in &self.stations {
            let station_spans = spans.remove(station.as_str()).unwrap_or_default();
            if !covers(station_spans, self.start, self.end) {
                return Err(Error::ObservationCoverage {
                    stations: vec![station.clone()],
                    reason: "successful source-history requests do not cover the full observation window".into(),
                });
            }
            if fresh.get(station.as_str()) != Some(&true) {
                return Err(Error::ObservationCoverage {
                    stations: vec![station.clone()],
                    reason: "the end of the observation window has not been checked by a request started after the signing deadline".into(),
                });
            }
        }
        Ok(())
    }
}

/// Whether `spans` together cover `start` to `end` without a gap.
fn covers(
    mut spans: Vec<(OffsetDateTime, OffsetDateTime)>,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> bool {
    spans.sort_unstable();
    let mut through = start;
    let mut covered_start = false;
    for (from, to) in spans {
        if to < start || from > through {
            continue;
        }
        covered_start = true;
        through = through.max(to);
    }
    covered_start && through >= end
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    fn requirement() -> Requirement {
        Requirement {
            start: datetime!(2026-01-17 00:00 UTC),
            end: datetime!(2026-01-17 06:00 UTC),
            collected_after: datetime!(2026-01-17 06:15 UTC),
            stations: vec!["KPWM".into()],
            groups: super::super::quality_groups(&[]),
            metrics: vec![],
        }
    }

    fn batch(start: &str, end: &str, requested: &str) -> serde_json::Value {
        json!({"station_ids":["KPWM"], "window_start":start, "window_end":end,
            "requested_at":requested, "completed_at":requested, "status":"complete",
            "source_url":"https://aviationweather.gov/api/data/metar?ids=KPWM&format=xml",
            "source_sha256":"a".repeat(64), "response_status":200,
            "response_count":3, "report_count":3, "error":null})
    }

    fn receipt(batches: Vec<serde_json::Value>) -> String {
        json!({"version":"awc-history-v1", "interval":"closed", "batches":batches}).to_string()
    }

    #[test]
    fn overlapping_history_and_a_fresh_tail_cover_a_long_event() {
        let json = receipt(vec![
            batch(
                "2026-01-17T00:00:00Z",
                "2026-01-17T03:00:00Z",
                "2026-01-17T03:01:00Z",
            ),
            batch(
                "2026-01-17T02:59:00Z",
                "2026-01-17T06:15:00Z",
                "2026-01-17T06:16:00Z",
            ),
        ]);
        assert!(requirement().verify_receipts(&[json]).is_ok());
    }

    #[test]
    fn a_gap_stale_request_missing_station_or_failed_response_cannot_authorize_settlement() {
        let good = batch(
            "2026-01-17T00:00:00Z",
            "2026-01-17T06:00:00Z",
            "2026-01-17T06:16:00Z",
        );
        for (key, bad) in [
            ("window_start", json!("2026-01-17T00:00:01Z")),
            ("window_end", json!("2026-01-17T05:59:59Z")),
            ("requested_at", json!("2026-01-17T06:14:59Z")),
            ("station_ids", json!(["KORD"])),
            ("status", json!("failed")),
            ("response_count", json!(400)),
        ] {
            let mut value = good.clone();
            value[key] = bad;
            assert!(
                requirement()
                    .verify_receipts(&[receipt(vec![value])])
                    .is_err(),
                "{key}"
            );
        }
        assert!(requirement().verify_receipts(&[]).is_err());
    }

    #[test]
    fn verified_empty_response_proves_collection_but_not_usable_measurements() {
        let mut value = batch(
            "2026-01-17T00:00:00Z",
            "2026-01-17T06:00:00Z",
            "2026-01-17T06:16:00Z",
        );
        value["status"] = json!("empty");
        value["response_status"] = json!(204);
        value["response_count"] = json!(0);
        value["report_count"] = json!(0);
        value["source_sha256"] = serde_json::Value::Null;
        assert!(
            requirement()
                .verify_receipts(&[receipt(vec![value])])
                .is_ok()
        );
    }

    #[test]
    fn a_received_defect_needs_a_newer_request_not_just_a_later_completion() {
        let mut defect = batch(
            "2026-01-17T00:00:00Z",
            "2026-01-17T06:00:00Z",
            "2026-01-17T06:17:00Z",
        );
        defect["status"] = json!("failed");
        defect["failure_kind"] = json!("representation");
        defect["error"] = json!("invalid coordinate");
        let mut stale = batch(
            "2026-01-17T00:00:00Z",
            "2026-01-17T06:00:00Z",
            "2026-01-17T06:16:00Z",
        );
        stale["completed_at"] = json!("2026-01-17T06:18:00Z");
        assert!(
            requirement()
                .verify_receipts(&[receipt(vec![stale, defect.clone()])])
                .is_err()
        );
        let fresh = batch(
            "2026-01-17T00:00:00Z",
            "2026-01-17T06:00:00Z",
            "2026-01-17T06:19:00Z",
        );
        assert!(
            requirement()
                .verify_receipts(&[receipt(vec![fresh, defect])])
                .is_ok()
        );
    }

    /// The daemon looks back four hours, so no single later request may span a long failed
    /// interval; several that overlap do.
    #[test]
    fn several_later_requests_together_cover_a_failed_interval() {
        let mut defect = batch(
            "2026-01-17T00:00:00Z",
            "2026-01-17T06:00:00Z",
            "2026-01-17T06:17:00Z",
        );
        defect["status"] = json!("failed");
        defect["failure_kind"] = json!("response");
        defect["error"] = json!("truncated body");
        let later = |start: &str, end: &str| batch(start, end, "2026-01-17T06:20:00Z");
        let pieces = vec![
            later("2026-01-17T00:00:00Z", "2026-01-17T02:00:00Z"),
            later("2026-01-17T02:00:00Z", "2026-01-17T04:10:00Z"),
            later("2026-01-17T04:00:00Z", "2026-01-17T06:15:00Z"),
        ];
        let mut with_defect = pieces.clone();
        with_defect.push(defect.clone());
        assert!(
            requirement()
                .verify_receipts(&[receipt(with_defect)])
                .is_ok()
        );
        // A gap between the later requests leaves the failure uncovered.
        let mut gapped = vec![pieces[0].clone(), pieces[2].clone(), defect.clone()];
        gapped.push(batch(
            "2026-01-17T02:00:00Z",
            "2026-01-17T04:10:00Z",
            "2026-01-17T06:16:00Z",
        ));
        assert!(requirement().verify_receipts(&[receipt(gapped)]).is_err());
        // Requests made before the failure do not count toward covering it.
        let mut early = pieces.clone();
        early[1]["requested_at"] = json!("2026-01-17T06:16:00Z");
        early[1]["completed_at"] = json!("2026-01-17T06:16:00Z");
        early.push(defect);
        assert!(requirement().verify_receipts(&[receipt(early)]).is_err());
    }
}
