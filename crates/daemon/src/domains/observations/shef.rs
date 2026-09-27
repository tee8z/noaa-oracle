//! Collect the bounded official ASOS RR7 product history. Failures remain in
//! the footer; they never prevent the independent METAR rows from publication.
use super::download_observations::sha256_hex;
use crate::{
    CityWeather, XmlFetcher,
    coordinates::{STATION_CATALOG_URL, StationCatalogEvidence},
};
use anyhow::{Result, anyhow, ensure};
use async_compression::tokio::bufread::GzipEncoder;
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::Duration as StdDuration,
};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::io::AsyncReadExt;

const LOCATIONS_URL: &str = "https://api.weather.gov/products/types/RR7/locations";
const PRODUCTS_URL: &str = "https://api.weather.gov/products";
const LIMIT: usize = 500;
const REQUESTS: usize = 512;
const BODY_BUDGET: usize = 8 * 1024 * 1024;
const COLLECTION_BUDGET: StdDuration = StdDuration::from_secs(10 * 60);
const SPACING: StdDuration = StdDuration::from_secs(1);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShefRow {
    pub station_id: String,
    pub source_station_id: String,
    pub start: String,
    pub end: String,
    pub liquid_in: Option<f64>,
    pub status: String,
    pub reason: Option<String>,
    pub source_url: String,
    pub source_sha256: String,
    pub issued_at: String,
    pub received_at: String,
    pub mapping_sha256: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct SourceDocument {
    pub source_url: String,
    pub received_at: String,
    pub encoding: String,
    pub body: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Issue {
    pub station_ids: Vec<String>,
    pub window_start: String,
    pub window_end: String,
    pub reason: String,
    pub kind: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct ShefCollection {
    pub version: String,
    pub requested_at: String,
    pub completed_at: String,
    pub supported_stations: Vec<String>,
    pub rows: Vec<ShefRow>,
    pub sources: BTreeMap<String, SourceDocument>,
    pub issues: Vec<Issue>,
}

#[async_trait]
trait Fetcher: Send + Sync {
    async fn request(&self, url: &str) -> Result<(u16, String)>;
}
#[async_trait]
impl Fetcher for XmlFetcher {
    async fn request(&self, url: &str) -> Result<(u16, String)> {
        Ok(self.fetch_shef_json(url).await?)
    }
}

struct FetchState<'a> {
    fetcher: &'a dyn Fetcher,
    deadline: tokio::time::Instant,
    next_request: tokio::time::Instant,
    spacing: StdDuration,
    requests: usize,
    max_requests: usize,
    bytes: usize,
}
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
struct FetchProblem {
    kind: &'static str,
    reason: String,
}
fn fetch_problem(kind: &'static str, reason: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(FetchProblem {
        kind,
        reason: reason.into(),
    })
}
impl FetchState<'_> {
    async fn get(
        &mut self,
        url: &str,
        output: &mut ShefCollection,
    ) -> Result<(String, String, OffsetDateTime)> {
        if self.requests >= self.max_requests
            || self.next_request >= self.deadline
            || tokio::time::Instant::now() >= self.deadline
            || self.bytes > BODY_BUDGET
        {
            return Err(fetch_problem(
                "budget",
                "RR7 request/time/evidence budget exhausted",
            ));
        }
        tokio::time::sleep_until(self.next_request).await;
        self.requests += 1;
        self.next_request = tokio::time::Instant::now() + self.spacing;
        let (status, body) =
            match tokio::time::timeout_at(self.deadline, self.fetcher.request(url)).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    return Err(fetch_problem(
                        "transport",
                        format!("RR7 request failed: {error:#}"),
                    ));
                }
                Err(error) => {
                    return Err(fetch_problem(
                        "budget",
                        format!("RR7 request deadline reached: {error}"),
                    ));
                }
            };
        let received = OffsetDateTime::now_utc();
        self.bytes += body.len();
        if self.bytes > BODY_BUDGET {
            return Err(fetch_problem(
                "budget",
                "RR7 evidence-size budget exhausted",
            ));
        }
        let hash = archive(&mut output.sources, url, &body, received).await?;
        if status != 200 {
            return Err(fetch_problem(
                "transport",
                format!("RR7 API returned HTTP {status}"),
            ));
        }
        Ok((body, hash, received))
    }
}

async fn archive(
    sources: &mut BTreeMap<String, SourceDocument>,
    url: &str,
    body: &str,
    received: OffsetDateTime,
) -> Result<String> {
    let hash = sha256_hex(body.as_bytes());
    let mut bytes = Vec::new();
    GzipEncoder::new(body.as_bytes())
        .read_to_end(&mut bytes)
        .await?;
    sources.entry(hash.clone()).or_insert(SourceDocument {
        source_url: url.into(),
        received_at: received.format(&Rfc3339)?,
        encoding: "gzip+base64".into(),
        body: STANDARD.encode(bytes),
    });
    Ok(hash)
}

pub async fn collect(
    fetcher: Arc<XmlFetcher>,
    stations: &CityWeather,
    catalog: &StationCatalogEvidence,
    started_at: OffsetDateTime,
    hours: u32,
) -> ShefCollection {
    collect_with(
        fetcher.as_ref(),
        stations,
        catalog,
        started_at,
        hours,
        SPACING,
        COLLECTION_BUDGET,
        REQUESTS,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn collect_with(
    fetcher: &dyn Fetcher,
    stations: &CityWeather,
    catalog: &StationCatalogEvidence,
    started_at: OffsetDateTime,
    hours: u32,
    spacing: StdDuration,
    budget: StdDuration,
    max_requests: usize,
) -> ShefCollection {
    let requested_at = OffsetDateTime::now_utc();
    let end = started_at.replace_nanosecond(0).unwrap_or(started_at) - Duration::SECOND;
    let start = end - Duration::hours(i64::from(hours));
    let mut output = ShefCollection {
        version: "asos-shef-v1".into(),
        requested_at: requested_at.format(&Rfc3339).unwrap_or_default(),
        completed_at: String::new(),
        supported_stations: Vec::new(),
        rows: Vec::new(),
        sources: BTreeMap::new(),
        issues: Vec::new(),
    };
    let mut fetch = FetchState {
        fetcher,
        deadline: tokio::time::Instant::now() + budget,
        next_request: tokio::time::Instant::now(),
        spacing,
        requests: 0,
        max_requests,
        bytes: catalog.raw_xml.len(),
    };
    if let Err(error) = collect_inner(
        &mut fetch,
        &mut output,
        stations,
        catalog,
        start,
        end,
        requested_at,
    )
    .await
    {
        issue(
            &mut output,
            stations.city_data.keys().cloned().collect(),
            start,
            end,
            error
                .downcast_ref::<FetchProblem>()
                .map(|failure| failure.kind)
                .unwrap_or("response"),
            format!("RR7 collection incomplete: {error:#}"),
        );
    }
    output.completed_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    output
}

fn issue(
    output: &mut ShefCollection,
    station_ids: Vec<String>,
    start: OffsetDateTime,
    end: OffsetDateTime,
    kind: &str,
    reason: String,
) {
    output.issues.push(Issue {
        station_ids,
        window_start: start.format(&Rfc3339).unwrap_or_default(),
        window_end: end.format(&Rfc3339).unwrap_or_default(),
        kind: kind.into(),
        reason,
    });
}

async fn collect_inner(
    fetch: &mut FetchState<'_>,
    output: &mut ShefCollection,
    stations: &CityWeather,
    catalog: &StationCatalogEvidence,
    start: OffsetDateTime,
    end: OffsetDateTime,
    query_end: OffsetDateTime,
) -> Result<()> {
    let mapping_hash = archive(
        &mut output.sources,
        STATION_CATALOG_URL,
        &catalog.raw_xml,
        catalog.received_at,
    )
    .await?;
    let aliases = noaa_oracle_core::shef::catalog_aliases(&catalog.raw_xml)?;
    let (body, _, _) = fetch.get(LOCATIONS_URL, output).await?;
    let locations: serde_json::Value = serde_json::from_str(&body)?;
    let locations = locations
        .get("locations")
        .and_then(|value| value.as_object())
        .ok_or_else(|| anyhow!("RR7 location discovery is malformed"))?;
    let selected: BTreeMap<String, String> = locations
        .keys()
        .filter_map(|location| {
            let mapped = aliases.get(location)?;
            (mapped.len() == 1 && stations.city_data.contains_key(&mapped[0]))
                .then(|| (location.clone(), mapped[0].clone()))
        })
        .collect();
    output.supported_stations = selected
        .values()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if selected.is_empty() {
        issue(
            output,
            stations.city_data.keys().cloned().collect(),
            start,
            end,
            "unmapped",
            "no current RR7 locations have an unambiguous configured station mapping".into(),
        );
        return Ok(());
    }
    // NWS rejects fractional seconds even though they are valid RFC3339.
    // Expand to whole-second query bounds; keep receipt timestamps exact.
    let query_end = query_end.replace_nanosecond(0)? + Duration::SECOND;
    let mut pending = VecDeque::from([(start - Duration::SECOND, query_end)]);
    let mut products = BTreeMap::<String, OffsetDateTime>::new();
    while let Some((from, to)) = pending.pop_front() {
        let url = index_url(&selected.keys().cloned().collect::<Vec<_>>(), from, to)?;
        let (body, _, _) = fetch.get(&url, output).await?;
        let index: ProductIndex = serde_json::from_str(&body)?;
        ensure!(
            index.products.len() <= LIMIT,
            "RR7 index exceeds requested limit"
        );
        if index.products.len() == LIMIT {
            ensure!(
                to - from > Duration::MINUTE,
                "RR7 index remains capped at minimum time interval"
            );
            let middle = from + Duration::seconds((to - from).whole_seconds() / 2);
            pending.push_front((middle, to));
            pending.push_front((from, middle));
            continue;
        }
        for product in index.products {
            ensure!(
                product.product_code == "RR7" && valid_id(&product.id),
                "RR7 index identity is malformed"
            );
            let issued = OffsetDateTime::parse(&product.issuance_time, &Rfc3339)?;
            ensure!(
                issued >= from && issued <= to,
                "RR7 index returned an out-of-window issue"
            );
            if let Some(previous) = products.insert(product.id, issued) {
                ensure!(
                    previous == issued,
                    "RR7 product has conflicting issue timestamps"
                );
            }
        }
    }
    let mut products: Vec<_> = products.into_iter().collect();
    // Preserve fresh forward intervals first when a long lookback exceeds the budget.
    // Any older missing interval still prevents a complete settlement chain.
    products.sort_by_key(|(_, issued)| std::cmp::Reverse(*issued));
    for (id, indexed_issue) in products {
        let url = format!("{PRODUCTS_URL}/{id}");
        let (body, hash, received) = match fetch.get(&url, output).await {
            Ok(value) => value,
            Err(error) => {
                issue(
                    output,
                    output.supported_stations.clone(),
                    start,
                    end,
                    error
                        .downcast_ref::<FetchProblem>()
                        .map(|failure| failure.kind)
                        .unwrap_or("response"),
                    format!("{url}: {error:#}"),
                );
                continue;
            }
        };
        let result = parse_product(
            &body,
            &id,
            indexed_issue,
            &aliases,
            &selected,
            start,
            end,
            &url,
            &hash,
            received,
            &mapping_hash,
        );
        match result {
            Ok(rows) => output.rows.extend(rows),
            Err(error) => issue(
                output,
                output.supported_stations.clone(),
                start,
                end,
                "response",
                format!("{url}: {error:#}"),
            ),
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct ProductIndex {
    #[serde(rename = "@graph")]
    products: Vec<ProductId>,
}
#[derive(Deserialize)]
struct ProductId {
    id: String,
    #[serde(rename = "productCode")]
    product_code: String,
    #[serde(rename = "issuanceTime")]
    issuance_time: String,
}
#[derive(Deserialize)]
struct Product {
    id: String,
    #[serde(rename = "productCode")]
    product_code: String,
    #[serde(rename = "issuanceTime")]
    issuance_time: String,
    #[serde(rename = "productText")]
    product_text: String,
}

fn valid_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}
fn index_url(locations: &[String], start: OffsetDateTime, end: OffsetDateTime) -> Result<String> {
    let mut url = reqwest::Url::parse(PRODUCTS_URL)?;
    url.query_pairs_mut()
        .append_pair("type", "RR7")
        .append_pair("location", &locations.join(","))
        .append_pair("start", &start.format(&Rfc3339)?)
        .append_pair("end", &end.format(&Rfc3339)?)
        .append_pair("limit", &LIMIT.to_string());
    Ok(url.into())
}

#[allow(clippy::too_many_arguments)]
fn parse_product(
    body: &str,
    id: &str,
    indexed_issue: OffsetDateTime,
    aliases: &BTreeMap<String, Vec<String>>,
    selected: &BTreeMap<String, String>,
    start: OffsetDateTime,
    end: OffsetDateTime,
    url: &str,
    hash: &str,
    received: OffsetDateTime,
    mapping_hash: &str,
) -> Result<Vec<ShefRow>> {
    let product: Product = serde_json::from_str(body)?;
    let issued = OffsetDateTime::parse(&product.issuance_time, &Rfc3339)?;
    ensure!(
        product.id == id && product.product_code == "RR7" && issued == indexed_issue,
        "RR7 product metadata conflicts with its index"
    );
    let allowed: BTreeSet<_> = selected.values().collect();
    let mut output = Vec::new();
    for value in noaa_oracle_core::shef::parse(&product.product_text, issued)? {
        let mapped = aliases
            .get(&value.source_station_id)
            .ok_or_else(|| anyhow!("RR7 source station is unmapped"))?;
        ensure!(
            mapped.len() == 1 && allowed.contains(&mapped[0]),
            "RR7 source station mapping is ambiguous or unexpected"
        );
        if value.end <= start || value.end > end {
            continue;
        }
        output.push(ShefRow {
            station_id: mapped[0].clone(),
            source_station_id: value.source_station_id,
            start: value.start.format(&Rfc3339)?,
            end: value.end.format(&Rfc3339)?,
            liquid_in: value.liquid_in,
            status: value.status,
            reason: value.reason,
            source_url: url.into(),
            source_sha256: hash.into(),
            issued_at: issued.format(&Rfc3339)?,
            received_at: received.format(&Rfc3339)?,
            mapping_sha256: mapping_hash.into(),
        });
    }
    Ok(output)
}

#[cfg(test)]
#[path = "shef_tests.rs"]
mod tests;
