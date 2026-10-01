//! Replay retained NWS fixed-hour precipitation and its station mapping before
//! using it. Liquid totals do not establish precipitation phase or snow depth.
use super::{Error, coverage, sql_string_list};
use base64::{Engine, engine::general_purpose::STANDARD};
use duckdb::Connection;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

const CATALOG_URL: &str = "https://aviationweather.gov/data/cache/stations.cache.xml.gz";
const PRODUCT_PREFIX: &str = "https://api.weather.gov/products/";
const MAX_BYTES: u64 = 8 * 1024 * 1024;
#[derive(Clone, Deserialize)]
struct Row {
    station_id: String,
    source_station_id: String,
    start: String,
    end: String,
    liquid_in: Option<f64>,
    status: String,
    reason: Option<String>,
    source_url: String,
    source_sha256: String,
    issued_at: String,
    received_at: String,
    mapping_sha256: String,
}
#[derive(Deserialize)]
struct Document {
    source_url: String,
    received_at: String,
    encoding: String,
    body: String,
}
#[derive(Deserialize)]
struct Issue {
    station_ids: Vec<String>,
    window_start: String,
    window_end: String,
    #[serde(default)]
    kind: Option<String>,
}
#[derive(Deserialize)]
struct Collection {
    version: String,
    requested_at: String,
    completed_at: String,
    supported_stations: Vec<String>,
    rows: Vec<Row>,
    sources: BTreeMap<String, Document>,
    issues: Vec<Issue>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Product {
    id: String,
    product_code: String,
    issuance_time: String,
    product_text: String,
}
#[derive(Clone, Debug)]
struct Measurement {
    start: OffsetDateTime,
    end: OffsetDateTime,
    issued: OffsetDateTime,
    received: OffsetDateTime,
    requested: OffsetDateTime,
    amount: Option<f64>,
}
#[derive(Default)]
pub(super) struct Evidence {
    rows: BTreeMap<String, Vec<Measurement>>,
    supported: BTreeSet<String>,
    defects: Vec<(Vec<String>, OffsetDateTime, OffsetDateTime, OffsetDateTime)>,
}
fn date(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok()
}
fn body(document: &Document, hash: &str, url: &str) -> Option<String> {
    if document.encoding != "gzip+base64"
        || document.source_url != url
        || document.body.len() > 2 * MAX_BYTES as usize
    {
        return None;
    }
    let compressed = STANDARD.decode(&document.body).ok()?;
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_BYTES as usize
        || Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            != hash
    {
        return None;
    }
    String::from_utf8(bytes).ok()
}
type Aliases = BTreeMap<String, Vec<String>>;
#[derive(Default)]
struct VerificationCache {
    // Compare compressed bytes before reuse: a later footer claiming the same
    // hash for a changed body must be checked again, not trusted from cache.
    catalogs: BTreeMap<String, (String, Option<Aliases>)>,
    products: BTreeMap<String, (String, Option<Product>)>,
}
fn verify(row: &Row, collection: &Collection, cache: &mut VerificationCache) -> Option<f64> {
    if row.status != "validated" || row.reason.is_some() {
        return None;
    }
    let id = row.source_url.strip_prefix(PRODUCT_PREFIX)?;
    if uuid::Uuid::parse_str(id).is_err() {
        return None;
    }
    let document = collection.sources.get(&row.source_sha256)?;
    if document.received_at != row.received_at
        || document.source_url != row.source_url
        || document.encoding != "gzip+base64"
        || document.body.len() > 2 * MAX_BYTES as usize
    {
        return None;
    }
    if cache
        .products
        .get(&row.source_sha256)
        .is_none_or(|(encoded, _)| encoded != &document.body)
    {
        let parsed = body(document, &row.source_sha256, &row.source_url)
            .and_then(|raw| serde_json::from_str::<Product>(&raw).ok());
        cache
            .products
            .insert(row.source_sha256.clone(), (document.body.clone(), parsed));
    }
    let product = cache.products.get(&row.source_sha256)?.1.as_ref()?;
    let issued = date(&row.issued_at)?;
    if product.id != id || product.product_code != "RR7" || date(&product.issuance_time)? != issued
    {
        return None;
    }
    let catalog = collection.sources.get(&row.mapping_sha256)?;
    if date(&catalog.received_at)? > date(&row.received_at)?
        || catalog.source_url != CATALOG_URL
        || catalog.encoding != "gzip+base64"
        || catalog.body.len() > 2 * MAX_BYTES as usize
    {
        return None;
    }
    if cache
        .catalogs
        .get(&row.mapping_sha256)
        .is_none_or(|(encoded, _)| encoded != &catalog.body)
    {
        let aliases = body(catalog, &row.mapping_sha256, CATALOG_URL)
            .and_then(|raw| noaa_oracle_core::shef::catalog_aliases(&raw).ok());
        cache
            .catalogs
            .insert(row.mapping_sha256.clone(), (catalog.body.clone(), aliases));
    }
    let aliases = cache.catalogs.get(&row.mapping_sha256)?.1.as_ref()?;
    if aliases.get(&row.source_station_id)? != &vec![row.station_id.clone()] {
        return None;
    }
    let parsed = noaa_oracle_core::shef::parse(&product.product_text, issued).ok()?;
    let matches: Vec<_> = parsed
        .iter()
        .filter(|value| {
            value.source_station_id == row.source_station_id
                && Some(value.start) == date(&row.start)
                && Some(value.end) == date(&row.end)
        })
        .collect();
    if matches.len() != 1
        || matches[0].status != "validated"
        || matches[0].liquid_in != row.liquid_in
    {
        return None;
    }
    row.liquid_in
        .filter(|value| value.is_finite() && (0.0..=5.0).contains(value))
}

impl Evidence {
    pub fn read(connection: &Connection, files: &[String]) -> Result<Self, Error> {
        if files.is_empty() {
            return Ok(Self::default());
        }
        let sql = format!(
            "SELECT decode(value) FROM parquet_kv_metadata([{}]) WHERE decode(key) = 'precipitation_observations'",
            sql_string_list(files)
        );
        let mut statement = connection.prepare(&sql)?;
        let receipts = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Self::decode(&receipts)
    }
    fn decode(receipts: &[String]) -> Result<Self, Error> {
        let mut output = Self::default();
        let now = OffsetDateTime::now_utc();
        let mut verification = VerificationCache::default();
        for json in receipts {
            let receipt: Collection =
                serde_json::from_str(json).map_err(|_| Error::QualityUnavailable)?;
            if receipt.version != "asos-shef-v1" {
                continue;
            }
            let requested = date(&receipt.requested_at).ok_or(Error::QualityUnavailable)?;
            let completed = date(&receipt.completed_at).ok_or(Error::QualityUnavailable)?;
            if requested > completed || completed > now {
                return Err(Error::QualityUnavailable);
            }
            output
                .supported
                .extend(receipt.supported_stations.iter().cloned());
            for issue in &receipt.issues {
                // Missing replies cannot invalidate already verified readings;
                // missing measurements still fail the exact interval chain.
                if matches!(
                    issue.kind.as_deref(),
                    Some("transport" | "budget" | "unmapped")
                ) {
                    continue;
                }
                let start = date(&issue.window_start).ok_or(Error::QualityUnavailable)?;
                let end = date(&issue.window_end).ok_or(Error::QualityUnavailable)?;
                output
                    .defects
                    .push((issue.station_ids.clone(), start, end, completed));
            }
            for row in &receipt.rows {
                let start = date(&row.start).ok_or(Error::QualityUnavailable)?;
                let end = date(&row.end).ok_or(Error::QualityUnavailable)?;
                let issued = date(&row.issued_at).ok_or(Error::QualityUnavailable)?;
                let received = date(&row.received_at).ok_or(Error::QualityUnavailable)?;
                let amount = if end - start == Duration::HOUR
                    && end.minute() == 0
                    && end.second() == 0
                    && end.nanosecond() == 0
                    && end <= issued
                    && issued <= received
                    && requested <= received
                    && received <= completed
                {
                    verify(row, &receipt, &mut verification)
                } else {
                    None
                };
                output
                    .rows
                    .entry(row.station_id.clone())
                    .or_default()
                    .push(Measurement {
                        start,
                        end,
                        issued,
                        received,
                        requested,
                        amount,
                    });
            }
        }
        Ok(output)
    }
    /// Some(None) means this source is present but cannot prove the requested
    /// total. It must not silently fall back to an older/different source.
    pub fn total(&self, station: &str, requirement: &coverage::Requirement) -> Option<Option<f64>> {
        if !self.supported.contains(station) && !self.rows.contains_key(station) {
            return None;
        }
        Some(self.complete_total(station, requirement))
    }
    fn complete_total(&self, station: &str, requirement: &coverage::Requirement) -> Option<f64> {
        let mut selected = BTreeMap::<OffsetDateTime, Measurement>::new();
        for row in self.rows.get(station)? {
            if row.start < requirement.start || row.end > requirement.end {
                continue;
            }
            match selected.get_mut(&row.start) {
                Some(old) if row.issued < old.issued => {}
                Some(old) if row.issued == old.issued => {
                    let amount = if old.amount == row.amount && old.end == row.end {
                        old.amount
                    } else {
                        None
                    };
                    if row.received > old.received {
                        *old = row.clone();
                    }
                    old.amount = amount;
                }
                _ => {
                    selected.insert(row.start, row.clone());
                }
            }
        }
        let mut cursor = requirement.start;
        let mut total = 0.0;
        while cursor < requirement.end {
            let row = selected.get(&cursor)?;
            if row.end <= cursor || row.end > requirement.end {
                return None;
            }
            if row.end == requirement.end && row.requested < requirement.collected_after {
                return None;
            }
            if self
                .defects
                .iter()
                .any(|(stations, start, end, completed)| {
                    stations.iter().any(|id| id == station)
                        && *start < row.end
                        && *end > row.start
                        && row.requested < *completed
                })
            {
                return None;
            }
            total += row.amount?;
            cursor = row.end;
        }
        (cursor == requirement.end && total.is_finite()).then_some(total)
    }
    pub fn recent_stations(&self, requested: &[String], now: OffsetDateTime) -> Vec<String> {
        requested
            .iter()
            .filter(|station| {
                self.rows.get(*station).is_some_and(|rows| {
                    rows.iter().any(|row| {
                        row.amount.is_some()
                            && row.end >= now - Duration::hours(3)
                            && row.end <= now
                            && row.received >= now - Duration::hours(3)
                    })
                })
            })
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Write;
    use time::macros::datetime;
    const PRODUCT: &str =
        include_str!("../../../daemon/src/domains/observations/testdata/rr7-nyc-positive.json");
    // Synthetic mapping for unit cases. Production's retained catalog has no
    // NYC alias; mci_fixture() below replays the complete official catalog.
    const CATALOG: &str = "<response><data_source name=\"stations\"/><errors/><warnings/><data num_results=\"1\"><Station><station_id>KNYC</station_id><iata_id>NYC</iata_id><country>US</country></Station></data></response>";
    fn archive(body: &str, url: &str) -> (String, Value) {
        let hash = Sha256::digest(body.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(body.as_bytes()).unwrap();
        (
            hash,
            json!({"source_url":url,"received_at":"2026-09-27T07:00:00Z","encoding":"gzip+base64","body":STANDARD.encode(gzip.finish().unwrap())}),
        )
    }
    pub(super) fn fixture() -> Value {
        let url = "https://api.weather.gov/products/458b17e0-d6d0-401a-9ddd-436d4fb0b137";
        let (hash, document) = archive(PRODUCT, url);
        let (mapping_hash, mapping) = archive(CATALOG, CATALOG_URL);
        json!({"version":"asos-shef-v1","requested_at":"2026-09-27T06:59:00Z","completed_at":"2026-09-27T07:01:00Z",
            "supported_stations":["KNYC"],"issues":[],"sources":{hash.clone():document,mapping_hash.clone():mapping},
            "rows":[{"station_id":"KNYC","source_station_id":"NYC","start":"2026-09-27T05:00:00Z","end":"2026-09-27T06:00:00Z",
                "liquid_in":0.14,"status":"validated","reason":null,"source_url":url,"source_sha256":hash,
                "issued_at":"2026-09-27T06:00:00Z","received_at":"2026-09-27T07:00:00Z","mapping_sha256":mapping_hash}]})
    }
    fn mci_fixture() -> Value {
        let product =
            include_str!("../../../daemon/src/domains/observations/testdata/rr7-mci-positive.json");
        let mut catalog = String::new();
        flate2::read::GzDecoder::new(
            include_bytes!(
                "../../../daemon/src/domains/observations/testdata/stations-20260927.xml.gz"
            )
            .as_slice(),
        )
        .read_to_string(&mut catalog)
        .unwrap();
        let aliases = noaa_oracle_core::shef::catalog_aliases(&catalog).unwrap();
        assert_eq!(aliases["MCI"], ["KMCI"]);
        assert!(!aliases.contains_key("NYC"));
        let url = "https://api.weather.gov/products/96ec7fd5-9bc1-434e-b77d-25e8754abc79";
        let (hash, mut document) = archive(product, url);
        let (mapping_hash, mut mapping) = archive(&catalog, CATALOG_URL);
        assert_eq!(
            mapping_hash,
            "8267824d92063561f7fbfccb507ab9ee4cdeed98240be0298bde595adc1bccc8"
        );
        document["received_at"] = json!("2026-09-26T09:00:00Z");
        mapping["received_at"] = json!("2026-09-26T08:57:00Z");
        json!({"version":"asos-shef-v1","requested_at":"2026-09-26T08:59:00Z","completed_at":"2026-09-26T09:01:00Z",
            "supported_stations":["KMCI"],"issues":[],"sources":{hash.clone():document,mapping_hash.clone():mapping},
            "rows":[{"station_id":"KMCI","source_station_id":"MCI","start":"2026-09-26T07:00:00Z","end":"2026-09-26T08:00:00Z",
                "liquid_in":0.21,"status":"validated","reason":null,"source_url":url,"source_sha256":hash,
                "issued_at":"2026-09-26T08:00:00Z","received_at":"2026-09-26T09:00:00Z","mapping_sha256":mapping_hash}]})
    }
    fn requirement() -> coverage::Requirement {
        coverage::Requirement {
            start: datetime!(2026-09-27 05:00 UTC),
            end: datetime!(2026-09-27 06:00 UTC),
            stations: vec!["KNYC".into()],
            collected_after: datetime!(2026-09-27 06:30 UTC),
            groups: super::super::quality_groups(&[]),
            metrics: vec![],
        }
    }
    fn total(receipts: &[Value], req: &coverage::Requirement) -> Option<Option<f64>> {
        Evidence::decode(&receipts.iter().map(Value::to_string).collect::<Vec<_>>())
            .unwrap()
            .total("KNYC", req)
    }
    #[test]
    fn actual_positive_hourly_source_is_replayed_and_cut_windows_are_unavailable() {
        let data = fixture();
        assert_eq!(
            total(std::slice::from_ref(&data), &requirement()),
            Some(Some(0.14))
        );
        let mut req = requirement();
        req.start += Duration::minutes(10);
        assert_eq!(total(std::slice::from_ref(&data), &req), Some(None));
        req = requirement();
        req.end += Duration::HOUR;
        assert_eq!(total(std::slice::from_ref(&data), &req), Some(None));
        req = requirement();
        req.collected_after += Duration::HOUR;
        assert_eq!(total(&[data], &req), Some(None));
    }
    #[test]
    fn altered_amount_hash_mapping_time_and_trace_are_not_trusted() {
        for (field, value) in [
            ("liquid_in", json!(0.15)),
            ("source_sha256", json!("0".repeat(64))),
            ("source_station_id", json!("JFK")),
            ("received_at", json!("2026-09-27T05:00:00Z")),
            ("status", json!("missing")),
            ("reason", json!("trace")),
            ("start", json!("2026-09-27T04:00:00Z")),
        ] {
            let mut data = fixture();
            data["rows"][0][field] = value;
            assert_eq!(total(&[data], &requirement()), Some(None), "{field}");
        }
    }
    #[test]
    fn contradictory_latest_issue_does_not_resurrect_older_measurement() {
        let valid = fixture();
        let mut bad = valid.clone();
        bad["rows"][0]["liquid_in"] = json!(0.7);
        for receipts in [[valid.clone(), bad.clone()], [bad.clone(), valid.clone()]] {
            assert_eq!(total(&receipts, &requirement()), Some(None));
        }
        bad["rows"][0]["issued_at"] = json!("2026-09-27T06:01:00Z");
        assert_eq!(total(&[valid, bad], &requirement()), Some(None));
    }
    #[test]
    fn failed_collection_requires_later_success_for_affected_intervals() {
        let valid = fixture();
        let mut failure = valid.clone();
        failure["rows"] = json!([]);
        failure["issues"] = json!([{"kind":"response","station_ids":["KNYC"],"window_start":"2026-09-27T05:00:00Z","window_end":"2026-09-27T06:00:00Z"}]);
        assert_eq!(
            total(&[valid.clone(), failure.clone()], &requirement()),
            Some(None)
        );
        failure["requested_at"] = json!("2026-09-27T06:40:00Z");
        failure["completed_at"] = json!("2026-09-27T06:50:00Z");
        assert_eq!(total(&[failure, valid], &requirement()), Some(Some(0.14)));
    }
    #[test]
    fn network_and_budget_issues_do_not_erase_verified_measurements() {
        for kind in ["transport", "budget", "unmapped"] {
            let mut failure = fixture();
            failure["rows"] = json!([]);
            failure["issues"] = json!([{"kind":kind,"station_ids":["KNYC"],"window_start":"2026-09-27T05:00:00Z","window_end":"2026-09-27T06:00:00Z"}]);
            assert_eq!(
                total(&[fixture(), failure], &requirement()),
                Some(Some(0.14)),
                "{kind}"
            );
        }
    }
    #[test]
    fn cached_verification_never_trusts_a_changed_body_with_the_same_claimed_hash() {
        for field in ["source_sha256", "mapping_sha256"] {
            let good: Collection = serde_json::from_value(fixture()).unwrap();
            let mut cache = VerificationCache::default();
            assert_eq!(verify(&good.rows[0], &good, &mut cache), Some(0.14));
            assert_eq!(verify(&good.rows[0], &good, &mut cache), Some(0.14));
            assert_eq!(cache.products.len(), 1);
            assert_eq!(cache.catalogs.len(), 1);
            let mut bad = fixture();
            let hash = bad["rows"][0][field].as_str().unwrap().to_owned();
            bad["sources"][hash]["body"] = json!("YQ==");
            let bad: Collection = serde_json::from_value(bad).unwrap();
            assert_eq!(verify(&bad.rows[0], &bad, &mut cache), None, "{field}");
        }
    }
    #[tokio::test]
    async fn ordinary_fifty_one_minute_metars_use_fixed_hour_precipitation() {
        use super::super::{
            ObservationRequest, TemperatureUnit, WeatherAccess, WeatherData, open_connection,
        };
        use std::sync::Arc;
        let directory = tempfile::tempdir().unwrap();
        let day = directory.path().join("2026-09-26");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join("observations_2026-09-26T09:01:00Z.parquet");
        let receipt = json!({"version":"awc-history-v1","interval":"closed","batches":[{
            "station_ids":["KMCI"],"window_start":"2026-09-26T06:00:00Z","window_end":"2026-09-26T08:59:59Z",
            "requested_at":"2026-09-26T09:00:00Z","completed_at":"2026-09-26T09:01:00Z","status":"complete",
            "source_url":"https://aviationweather.gov/api/data/metar?ids=KMCI&format=xml","source_sha256":"a".repeat(64),
            "response_status":200,"response_count":3,"report_count":3,"error":null}]}).to_string().replace('\'',"''");
        let shef = mci_fixture().to_string().replace('\'', "''");
        let connection = open_connection().unwrap();
        connection.execute_batch(&format!(r#"COPY (
            SELECT 'KMCI' AS station_id, generated_at, 15.0::DOUBLE AS temperature_value,'celsius' AS temperature_unit_code,
                12.0::DOUBLE AS dewpoint_value,'celsius' AS dewpoint_unit_code,8::BIGINT AS wind_speed,'knots' AS wind_speed_unit_code,
                90::BIGINT AS wind_direction,'degrees true' AS wind_direction_unit_code,0.1::DOUBLE AS precip_in,'inches' AS precip_unit_code,
                'RA' AS wx_string,'METAR' AS metar_type,raw_text,'validated' AS quality_status,'metar-consistency-v1' AS validation_version
            FROM (VALUES
                ('2026-09-26T06:51:00Z','METAR KMCI 260651Z 09008KT 10SM RA 15/12 RMK AO2 P0010'),
                ('2026-09-26T07:51:00Z','METAR KMCI 260751Z 09008KT 10SM RA 15/12 RMK AO2 P0010'),
                ('2026-09-26T08:51:00Z','METAR KMCI 260851Z 09008KT 10SM RA 15/12 RMK AO2 P0010')
            ) AS reports(generated_at,raw_text)
        ) TO '{}' (FORMAT PARQUET,KV_METADATA {{observation_coverage:'{receipt}',precipitation_observations:'{shef}'}})"#,path.display())).unwrap();
        let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
            directory.path().to_string_lossy().into_owned(),
        )));
        // METAR rows isolate phase/coverage behavior; the positive amount and
        // station mapping come from unchanged official source documents.
        let req = ObservationRequest {
            start: Some(datetime!(2026-09-26 07:00 UTC)),
            end: Some(datetime!(2026-09-26 08:00 UTC)),
            station_ids: "KMCI".into(),
            temperature_unit: TemperatureUnit::Celsius,
        };
        let rows = access
            .settlement_observations(
                &req,
                vec!["KMCI".into()],
                datetime!(2026-09-26 08:30 UTC),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rain_amt, Some(0.21));
        assert_eq!(rows[0].temp_high, Some(15.0));
    }
}
