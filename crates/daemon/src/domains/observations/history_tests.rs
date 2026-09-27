use super::*;
use crate::{CityWeather, ObservationService, RateLimiter, WeatherStation};
use parquet::file::reader::{FileReader, SerializedFileReader};
use std::sync::Mutex;

struct Mock {
    responses: Mutex<VecDeque<Result<(u16, String)>>>,
    urls: Mutex<Vec<String>>,
}
impl Mock {
    fn new(responses: Vec<Result<(u16, String)>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            urls: Mutex::new(Vec::new()),
        }
    }
}
#[async_trait]
impl HistoryFetcher for Mock {
    async fn request(&self, url: &str) -> Result<(u16, String)> {
        self.urls.lock().unwrap().push(url.into());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected API request")
    }
}
fn time(value: &str) -> OffsetDateTime {
    OffsetDateTime::parse(value, &Rfc3339).unwrap()
}
fn start() -> OffsetDateTime {
    time("2026-09-24T19:00:01Z")
}
fn report(station: &str, kind: &str, stamp: &str, raw_stamp: &str) -> String {
    format!(
        "<METAR><raw_text>{kind} {station} {raw_stamp} 06003KT 10SM CLR 19/06 A3044 RMK AO2 T01890056</raw_text><station_id>{station}</station_id><observation_time>{stamp}</observation_time><latitude>43.65</latitude><longitude>-70.31</longitude><temp_c>18.9</temp_c><dewpoint_c>5.6</dewpoint_c><wind_dir_degrees>60</wind_dir_degrees><wind_speed_kt>3</wind_speed_kt><metar_type>{kind}</metar_type></METAR>"
    )
}
fn response(reports: &[String]) -> String {
    format!(
        "<response><request_index>1</request_index><data_source name=\"metar\"/><request type=\"retrieve\"/><errors/><warnings/><time_taken_ms>1</time_taken_ms><data num_results=\"{}\">{}</data></response>",
        reports.len(),
        reports.join("")
    )
}
async fn run(mock: &Mock, stations: &[&str], max_requests: usize) -> HistoryCollection {
    collect(
        mock,
        stations.iter().map(|s| s.to_string()).collect(),
        start(),
        &HistoryConfig {
            hours: 3,
            ..HistoryConfig::default()
        },
        StdDuration::ZERO,
        StdDuration::from_secs(5),
        max_requests,
    )
    .await
    .unwrap()
}
fn service() -> ObservationService {
    let logger = slog::Logger::root(slog::Discard, slog::o!());
    let limiter = Arc::new(tokio::sync::Mutex::new(RateLimiter::new(
        1,
        StdDuration::from_secs(1),
    )));
    ObservationService::new(
        logger.clone(),
        Arc::new(XmlFetcher::new(logger, "history-test", limiter).unwrap()),
    )
}
fn stations() -> CityWeather {
    CityWeather {
        city_data: [(
            "KPWM".into(),
            WeatherStation {
                station_id: "KPWM".into(),
                station_name: "Portland".into(),
                state: "ME".into(),
                iata_id: "PWM".into(),
                elevation_m: None,
                latitude: "43.65".into(),
                longitude: "-70.31".into(),
            },
        )]
        .into(),
    }
}
fn metadata(path: &std::path::Path, name: &str) -> serde_json::Value {
    let reader = SerializedFileReader::new(std::fs::File::open(path).unwrap()).unwrap();
    let metadata = reader
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .unwrap();
    serde_json::from_str(
        metadata
            .iter()
            .find(|entry| entry.key == name)
            .unwrap()
            .value
            .as_ref()
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn keeps_all_routine_and_special_reports_and_archives_original_response() {
    let body = response(&[
        report("KPWM", "METAR", "2026-09-24T16:00:00Z", "241600Z"),
        report("KPWM", "SPECI", "2026-09-24T18:22:00Z", "241822Z"),
        report("KPWM", "METAR", "2026-09-24T19:00:00Z", "241900Z"),
        report("KPWM", "METAR", "2026-09-24T19:00:01Z", "241900Z"),
    ]);
    let mock = Mock::new(vec![Ok((200, body.clone()))]);
    let history = run(&mock, &["KPWM"], 10).await;
    assert_eq!(
        history.reports.len(),
        3,
        "padding report must not expand certified window"
    );
    let receipt = &history.coverage.batches[0];
    assert_eq!(receipt.status, "complete");
    assert_eq!(receipt.response_count, 4);
    assert_eq!(receipt.report_count, 3);
    assert_eq!(receipt.window_start, "2026-09-24T16:00:00Z");
    assert_eq!(receipt.window_end, "2026-09-24T19:00:00Z");
    let url = reqwest::Url::parse(&mock.urls.lock().unwrap()[0]).unwrap();
    let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["date"], "2026-09-24T19:00:01Z");
    assert!(params["hours"].parse::<f64>().unwrap() > 3.0);
    let bytes = STANDARD
        .decode(&history.sources[&sha256_hex(body.as_bytes())].body)
        .unwrap();
    let mut decoded = String::new();
    async_compression::tokio::bufread::GzipDecoder::new(bytes.as_slice())
        .read_to_string(&mut decoded)
        .await
        .unwrap();
    assert_eq!(decoded, body);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observations.parquet");
    let written = service()
        .write_history(&stations(), path.to_str().unwrap(), &history)
        .unwrap();
    assert_eq!(written.written, 3);
    assert_eq!(written.rejected, 0);
    assert_eq!(
        metadata(&path, "observation_coverage")["batches"][0]["status"],
        "complete"
    );
    assert!(metadata(&path, "observation_audit")["source_documents"][sha256_hex(body.as_bytes())]["body"].is_string());
}

#[tokio::test]
async fn successful_empty_is_distinct_from_transport_http_and_parser_failure() {
    for (reply, expected) in [
        (Ok((204, String::new())), "empty"),
        (Ok((200, response(&[]))), "empty"),
        (Ok((200, String::new())), "failed"),
        (Ok((503, "upstream unavailable".into())), "failed"),
        (Err(anyhow!("connection timed out")), "failed"),
        (Ok((204, "unexpected data".into())), "failed"),
    ] {
        let history = run(&Mock::new(vec![reply]), &["KPWM"], 10).await;
        assert_eq!(history.coverage.batches[0].status, expected);
        assert!(history.reports.is_empty());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("observations.parquet");
        service()
            .write_history(&stations(), path.to_str().unwrap(), &history)
            .unwrap();
        let reader = SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(reader.metadata().file_metadata().num_rows(), 0);
        assert_eq!(
            metadata(&path, "observation_coverage")["batches"][0]["status"],
            expected
        );
    }
}

#[tokio::test]
async fn malformed_count_station_and_time_never_certify_coverage() {
    let good = response(&[report("KPWM", "METAR", "2026-09-24T18:51:00Z", "241851Z")]);
    for body in [
        good.replace("num_results=\"1\"", "num_results=\"2\""),
        good.replace("KPWM", "KJFK"),
        good.replace("2026-09-24T18:51:00Z", "2026-09-24T14:51:00Z"),
        good.replace("2026-09-24T18:51:00Z", "bad timestamp"),
        good.replace("<warnings/>", "<warnings>truncated</warnings>"),
    ] {
        let history = run(&Mock::new(vec![Ok((200, body))]), &["KPWM"], 10).await;
        assert_eq!(history.coverage.batches[0].status, "failed");
        assert!(history.coverage.batches[0].error.is_some());
        assert_eq!(
            history.coverage.batches[0].failure_kind.as_deref(),
            Some("response")
        );
        assert_eq!(
            history.sources.len(),
            1,
            "bad source response must remain auditable"
        );
    }
}

#[tokio::test]
async fn cap_queries_split_and_only_successful_children_certify() {
    let one = report("KPWM", "METAR", "2026-09-24T18:51:00Z", "241851Z");
    let capped = response(&vec![one.clone(); RESPONSE_CAP]);
    let mock = Mock::new(vec![
        Ok((200, capped.clone())),
        Ok((200, response(&[one]))),
        Ok((204, String::new())),
    ]);
    let history = run(&mock, &["KPWM", "KZZZ"], 3).await;
    assert_eq!(history.reports.len(), 1);
    assert_eq!(
        history
            .coverage
            .batches
            .iter()
            .map(|batch| batch.status.as_str())
            .collect::<Vec<_>>(),
        ["failed", "complete", "empty"]
    );
    assert_eq!(history.coverage.batches[1].station_ids, ["KPWM"]);
    assert_eq!(history.coverage.batches[2].station_ids, ["KZZZ"]);
    let exhausted = run(&Mock::new(vec![Ok((200, capped))]), &["KPWM"], 1).await;
    assert_eq!(
        exhausted.coverage.batches.len(),
        3,
        "single-station cap splits time"
    );
    assert!(
        exhausted
            .coverage
            .batches
            .iter()
            .all(|batch| batch.status == "failed")
    );
    assert_eq!(
        exhausted.coverage.batches[1].window_end,
        exhausted.coverage.batches[2].window_start
    );
    assert_eq!(exhausted.sources.len(), 1);
}

#[tokio::test]
async fn unrepresentable_reports_keep_evidence_and_fail_the_whole_batch_interval() {
    let valid = report("KPWM", "METAR", "2026-09-24T18:51:00Z", "241851Z");
    let invalid = valid.replace("<latitude>43.65</latitude>", "<latitude>-99.99</latitude>");
    let history = run(
        &Mock::new(vec![Ok((200, response(&[valid, invalid])))]),
        &["KPWM"],
        10,
    )
    .await;
    assert_eq!(history.coverage.batches[0].status, "failed");
    assert_eq!(history.reports.len(), 2);
    assert_eq!(
        history.coverage.batches[0].failure_kind.as_deref(),
        Some("representation")
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observations.parquet");
    let written = service()
        .write_history(&stations(), path.to_str().unwrap(), &history)
        .unwrap();
    assert_eq!(written.written, 1);
    assert_eq!(written.skipped, 1);
    assert_eq!(metadata(&path, "observation_audit")["unrepresentable"], 1);
    assert_eq!(
        metadata(&path, "observation_coverage")["batches"][0]["status"],
        "failed"
    );
}

#[test]
fn invalid_history_config_and_catalog_failure_cannot_claim_empty_coverage() {
    assert!(
        HistoryConfig {
            hours: 0,
            batch_size: 25
        }
        .validate()
        .is_err()
    );
    assert!(
        HistoryConfig {
            hours: 3,
            batch_size: 26
        }
        .validate()
        .is_err()
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observations.parquet");
    service()
        .write_unavailable_history(
            path.to_str().unwrap(),
            start(),
            &HistoryConfig::default(),
            "station catalog unavailable".into(),
        )
        .unwrap();
    let coverage = metadata(&path, "observation_coverage");
    assert_eq!(coverage["batches"][0]["status"], "failed");
    assert_eq!(coverage["batches"][0]["station_ids"], serde_json::json!([]));
}

// Official AWC history response retrieved 2026-09-27 with ids=KPWM,KJFK,
// date=2026-09-24T19:00:01Z, hours=3.00055556, format=xml.
#[tokio::test]
async fn real_awc_history_response_preserves_all_reports_and_the_bad_kpwm_decode() {
    let xml = include_str!("testdata/history-kpwm-kjfk-20260924.xml");
    let history = run(
        &Mock::new(vec![Ok((200, xml.into()))]),
        &["KPWM", "KJFK"],
        10,
    )
    .await;
    assert_eq!(history.coverage.batches[0].status, "complete");
    assert_eq!(history.coverage.batches[0].report_count, 6);
    let kpwm = history
        .reports
        .iter()
        .find(|report| report.station_id == "KPWM" && report.temp_c.as_deref() == Some("60"))
        .unwrap();
    let weather = CurrentWeather::try_from(kpwm.clone()).unwrap();
    assert_eq!(weather.quality_status, "rejected");
    assert_eq!(weather.temperature_value, Some(60.0));
    assert_eq!(weather.metar_type.as_deref(), Some("METAR"));
}

#[tokio::test]
async fn successful_padding_only_response_keeps_original_count_and_zero_retained_rows() {
    let xml = response(&[report("KPWM", "METAR", "2026-09-24T19:00:01Z", "241900Z")]);
    let history = run(&Mock::new(vec![Ok((200, xml))]), &["KPWM"], 10).await;
    assert_eq!(history.coverage.batches[0].status, "complete");
    assert_eq!(history.coverage.batches[0].response_count, 1);
    assert_eq!(history.coverage.batches[0].report_count, 0);
    assert!(history.reports.is_empty());
}
