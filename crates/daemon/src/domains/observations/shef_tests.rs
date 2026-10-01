use super::*;
use crate::WeatherStation;
use std::sync::Mutex;
use time::macros::datetime;

const PRODUCT: &str = include_str!("testdata/rr7-nyc-positive.json");
// Synthetic NYC mapping isolates parser/failure cases. The retained live
// catalog has no NYC alias; the KMCI integration below uses its original bytes.
const CATALOG: &str = "<response><data_source name=\"station\"/><errors/><warnings/><data num_results=\"1\"><Station><station_id>KNYC</station_id><iata_id>NYC</iata_id><country>US</country></Station></data></response>";
struct Mock {
    responses: Mutex<VecDeque<Result<(u16, String)>>>,
    urls: Mutex<Vec<String>>,
}
#[async_trait]
impl Fetcher for Mock {
    async fn request(&self, url: &str) -> Result<(u16, String)> {
        self.urls.lock().unwrap().push(url.into());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request")
    }
}
fn assert_integral_request_times(mock: &Mock) {
    let urls = mock.urls.lock().unwrap();
    let index_url = reqwest::Url::parse(&urls[1]).unwrap();
    for (key, value) in index_url
        .query_pairs()
        .filter(|(key, _)| key == "start" || key == "end")
    {
        assert_eq!(
            OffsetDateTime::parse(&value, &Rfc3339)
                .unwrap()
                .nanosecond(),
            0,
            "NWS rejects fractional seconds in {key}"
        );
    }
}

fn stations() -> CityWeather {
    CityWeather {
        city_data: [(
            "KNYC".into(),
            WeatherStation {
                station_id: "KNYC".into(),
                station_name: "Central Park".into(),
                state: "NY".into(),
                iata_id: "NYC".into(),
                elevation_m: None,
                latitude: "40.78".into(),
                longitude: "-73.97".into(),
                reports_metar: true,
            },
        )]
        .into(),
    }
}
fn index() -> String {
    serde_json::json!({"@graph":[{"id":"458b17e0-d6d0-401a-9ddd-436d4fb0b137","productCode":"RR7","issuanceTime":"2026-09-27T06:00:00+00:00"}]}).to_string()
}
async fn run(replies: Vec<Result<(u16, String)>>, max_requests: usize) -> (ShefCollection, Mock) {
    let mock = Mock {
        responses: Mutex::new(replies.into()),
        urls: Mutex::new(Vec::new()),
    };
    let catalog = StationCatalogEvidence {
        raw_xml: CATALOG.into(),
        received_at: datetime!(2026-09-27 07:00 UTC),
    };
    let output = collect_with(
        &mock,
        &stations(),
        &catalog,
        datetime!(2026-09-27 07:00:01 UTC),
        3,
        StdDuration::ZERO,
        StdDuration::from_secs(5),
        max_requests,
    )
    .await;
    (output, mock)
}
fn discovery() -> Result<(u16, String)> {
    Ok((200, "{\"locations\":{\"NYC\":null,\"ANC\":null}}".into()))
}

#[tokio::test]
async fn actual_positive_shef_report_keeps_utc_interval_mapping_and_replay_evidence() {
    let (output, mock) = run(
        vec![discovery(), Ok((200, index())), Ok((200, PRODUCT.into()))],
        10,
    )
    .await;
    assert!(output.issues.is_empty(), "{:?}", output.issues);
    assert_eq!(output.rows.len(), 1);
    let row = &output.rows[0];
    assert_eq!(row.station_id, "KNYC");
    assert_eq!(row.source_station_id, "NYC");
    assert_eq!(row.start, "2026-09-27T05:00:00Z");
    assert_eq!(row.end, "2026-09-27T06:00:00Z");
    assert_eq!(row.liquid_in, Some(0.14));
    assert_eq!(row.status, "validated");
    assert_eq!(row.source_sha256, sha256_hex(PRODUCT.as_bytes()));
    assert_eq!(row.mapping_sha256, sha256_hex(CATALOG.as_bytes()));
    assert_eq!(output.supported_stations, ["KNYC"]);
    assert_eq!(mock.urls.lock().unwrap().len(), 3);
    assert_integral_request_times(&mock);
    let encoded = &output.sources[&row.source_sha256].body;
    let bytes = STANDARD.decode(encoded).unwrap();
    let mut decoded = String::new();
    async_compression::tokio::bufread::GzipDecoder::new(bytes.as_slice())
        .read_to_string(&mut decoded)
        .await
        .unwrap();
    assert_eq!(decoded, PRODUCT);
}

#[tokio::test]
async fn actual_kmci_positive_bulletin_and_full_official_catalog_are_replayable() {
    const MCI: &str = include_str!("testdata/rr7-mci-positive.json");
    let mut raw_xml = String::new();
    async_compression::tokio::bufread::GzipDecoder::new(
        include_bytes!("testdata/stations-20260927.xml.gz").as_slice(),
    )
    .read_to_string(&mut raw_xml)
    .await
    .unwrap();
    assert_eq!(
        sha256_hex(raw_xml.as_bytes()),
        "8267824d92063561f7fbfccb507ab9ee4cdeed98240be0298bde595adc1bccc8"
    );
    let aliases = noaa_oracle_core::shef::catalog_aliases(&raw_xml).unwrap();
    assert_eq!(aliases["MCI"], ["KMCI"]);
    assert!(!aliases.contains_key("NYC"), "do not invent a KNYC mapping");
    let mock = Mock {
        responses: Mutex::new(VecDeque::from([
            Ok((200, "{\"locations\":{\"MCI\":null}}".into())),
            Ok((200, serde_json::json!({"@graph":[{"id":"96ec7fd5-9bc1-434e-b77d-25e8754abc79","productCode":"RR7","issuanceTime":"2026-09-26T08:00:00+00:00"}]}).to_string())),
            Ok((200, MCI.into())),
        ])),
        urls: Mutex::new(Vec::new()),
    };
    let mut selected = stations();
    let mut station = selected.city_data.remove("KNYC").unwrap();
    station.station_id = "KMCI".into();
    station.iata_id = "MCI".into();
    selected.city_data.insert("KMCI".into(), station);
    let catalog = StationCatalogEvidence {
        raw_xml,
        received_at: datetime!(2026-09-27 14:47 UTC),
    };
    let output = collect_with(
        &mock,
        &selected,
        &catalog,
        datetime!(2026-09-26 09:00:01 UTC),
        3,
        StdDuration::ZERO,
        StdDuration::from_secs(5),
        10,
    )
    .await;
    assert!(output.issues.is_empty(), "{:?}", output.issues);
    assert_eq!(output.supported_stations, ["KMCI"]);
    assert_eq!(output.rows.len(), 1);
    let row = &output.rows[0];
    assert_eq!(row.station_id, "KMCI");
    assert_eq!(row.source_station_id, "MCI");
    assert_eq!(row.start, "2026-09-26T07:00:00Z");
    assert_eq!(row.end, "2026-09-26T08:00:00Z");
    assert_eq!(row.liquid_in, Some(0.21));
    assert_eq!(row.status, "validated");
    assert_eq!(row.source_sha256, sha256_hex(MCI.as_bytes()));
    assert_eq!(row.mapping_sha256, sha256_hex(catalog.raw_xml.as_bytes()));
    assert!(output.sources.contains_key(&row.source_sha256));
    assert!(output.sources.contains_key(&row.mapping_sha256));
}

#[tokio::test]
async fn missing_trace_and_malformed_amounts_stay_visible_without_zero_substitution() {
    for (raw, status) in [("M", "missing"), ("T", "missing"), ("NaN", "rejected")] {
        let product = PRODUCT.replace("PPH 0.14", &format!("PPH {raw}"));
        let (output, _) = run(
            vec![discovery(), Ok((200, index())), Ok((200, product))],
            10,
        )
        .await;
        assert_eq!(output.rows.len(), 1);
        assert_eq!(output.rows[0].status, status);
        assert_eq!(output.rows[0].liquid_in, None);
        assert!(output.rows[0].reason.is_some());
    }
}

#[tokio::test]
async fn malformed_source_or_changed_identity_is_not_a_missing_zero_hour() {
    for product in [
        PRODUCT.replace(".A NYC", ".A ABC"),
        PRODUCT.replace("RR7\"", "RR6\""),
        PRODUCT.replace("DH0600", "DH0651"),
        PRODUCT.replace("0.14", "0.14/PPD 0.20"),
    ] {
        let (output, _) = run(
            vec![discovery(), Ok((200, index())), Ok((200, product))],
            10,
        )
        .await;
        assert!(output.rows.is_empty());
        assert!(output.issues.iter().any(|issue| issue.kind == "response"));
        assert_eq!(output.sources.len(), 4, "bad response remains archived");
    }
}

#[tokio::test]
async fn outage_and_budget_produce_failed_receipts_instead_of_rows() {
    let (output, _) = run(vec![Err(anyhow!("NWS offline"))], 10).await;
    assert!(output.rows.is_empty());
    assert!(!output.issues.is_empty());
    let (output, mock) = run(vec![discovery(), Ok((200, index()))], 2).await;
    assert_eq!(mock.urls.lock().unwrap().len(), 2);
    assert!(output.rows.is_empty());
    assert!(
        output
            .issues
            .iter()
            .any(|issue| issue.reason.contains("budget"))
    );
    let (output, _) = run(vec![Ok((200, "{\"locations\":{\"ANC\":null}}".into()))], 10).await;
    assert!(output.supported_stations.is_empty());
    assert_eq!(output.issues[0].kind, "unmapped");
}
