use super::*;

// The Sep 24 KPWM report that the upstream decoder turned into 60 C / 3 C.
// The slash in the malformed wind must not be mistaken for temperature/dewpoint.
const KPWM: &str =
    "METAR KPWM 241851Z COR 060/03 10SM FEW220 19/06 A3044 RMK AO2 SLP308 T01890056 $";

fn metar(raw: &str, temperature: Option<&str>, dewpoint: Option<&str>) -> Metar {
    Metar {
        raw_text: raw.into(),
        metar_type: None,
        station_id: "KPWM".into(),
        observation_time: Some("2026-09-24T18:51:00Z".into()),
        latitude: Some("43.65".into()),
        longitude: Some("-70.31".into()),
        temp_c: temperature.map(str::to_owned),
        dewpoint_c: dewpoint.map(str::to_owned),
        wind_dir_degrees: Some("60".into()),
        wind_speed_kt: Some("3".into()),
        elevation_m: None,
        wx_string: None,
        precip_in: None,
    }
}

#[test]
fn rejects_the_kpwm_decoded_140_f_report_without_replacing_its_reading() {
    let report = metar(KPWM, Some("60"), Some("3"));
    let weather = CurrentWeather::try_from(report).unwrap();
    assert_eq!(weather.quality_status, "rejected");
    assert_eq!(weather.temperature_value, Some(60.0));
    assert_eq!(weather.raw_text, KPWM);
    let message = weather.quality_reason.unwrap();
    assert!(message.contains("decoded temperature 60 C"), "{message}");
    assert!(message.contains("T01890056 (18.9 C)"), "{message}");

    // The body alone catches this decoder error too; 060/03 is not its evidence.
    let body = KPWM.split_once(" RMK ").unwrap().0;
    let weather = CurrentWeather::try_from(metar(body, Some("60"), Some("3"))).unwrap();
    assert_eq!(weather.quality_status, "rejected");
    assert!(weather.quality_reason.unwrap().contains("19/06 (19 C)"));
}

#[test]
fn checks_dewpoint_even_when_the_decoded_temperature_is_correct() {
    let weather = CurrentWeather::try_from(metar(KPWM, Some("18.9"), Some("3"))).unwrap();
    assert_eq!(weather.quality_status, "rejected");
    let reason = weather.quality_reason.unwrap();
    assert!(reason.contains("decoded dewpoint 3 C"));
    assert!(reason.contains("T01890056 (5.6 C)"));
}

#[test]
fn matching_decoded_precision_and_whole_degree_values_are_kept_unchanged() {
    for (temperature, dewpoint) in [("18.9", "5.6"), ("19", "6")] {
        let weather =
            CurrentWeather::try_from(metar(KPWM, Some(temperature), Some(dewpoint))).unwrap();
        assert_eq!(
            weather.temperature_value,
            Some(temperature.parse().unwrap())
        );
        assert_eq!(weather.dewpoint_value, Some(dewpoint.parse().unwrap()));
        assert_eq!(weather.wind_direction, Some(60));
        assert_eq!(weather.wind_speed, Some(3));
    }
}

#[test]
fn standard_groups_allow_rounding_including_negative_and_half_degree_values() {
    for (group, temperature, dewpoint) in [
        ("03/M01", "2.6", "-1.5"),
        ("M03/M06", "-2.6", "-5.6"),
        ("M00/M00", "-0.5", "-0.2"),
        ("00/00", "0.5", "0"),
        // This is a consistency check, not a blanket maximum-temperature cutoff.
        ("55/03", "55", "3"),
    ] {
        let raw = format!("METAR KPWM 241851Z 06003KT 10SM CLR {group} A3044");
        let weather =
            CurrentWeather::try_from(metar(&raw, Some(temperature), Some(dewpoint))).unwrap();
        assert_eq!(
            weather.temperature_value,
            Some(temperature.parse().unwrap())
        );
        assert_eq!(weather.dewpoint_value, Some(dewpoint.parse().unwrap()));
    }
    let raw = "METAR KPWM 241851Z 06003KT 10SM CLR 19/06 A3044";
    assert_eq!(
        CurrentWeather::try_from(metar(raw, Some("19.6"), Some("6")))
            .unwrap()
            .quality_status,
        "rejected"
    );
}

#[test]
fn precise_remarks_decode_both_signs_and_are_preferred_over_body_groups() {
    let raw = "METAR KPWM 241851Z 06003KT 10SM CLR M03/M01 A3044 RMK AO2 T10261015=";
    let temperatures = raw_temperatures(raw).unwrap();
    assert_eq!(temperatures.temperature, -2.6);
    assert_eq!(temperatures.dewpoint, Some(-1.5));
    assert_eq!(temperatures.token, "T10261015");
    let weather = CurrentWeather::try_from(metar(raw, Some("-2.6"), Some("-1.5"))).unwrap();
    assert_eq!(weather.temperature_value, Some(-2.6));
    assert_eq!(weather.dewpoint_value, Some(-1.5));
    assert_eq!(
        CurrentWeather::try_from(metar(raw, Some("2.6"), Some("-1.5")))
            .unwrap()
            .quality_status,
        "rejected"
    );

    let zero = raw_temperatures("METAR KPWM 241851Z M00/M00 A3044 RMK T10001002").unwrap();
    assert!(zero.temperature.is_sign_negative());
    assert_eq!(zero.dewpoint, Some(-0.2));
}

#[test]
fn missing_raw_or_decoded_values_remain_missing_without_substitution() {
    for raw in [
        "METAR KPWM 241851Z 06003KT 10SM CLR 19/ A3044",
        "METAR KPWM 241851Z 06003KT 10SM CLR A3044 RMK AO2 T0189",
    ] {
        let weather = CurrentWeather::try_from(metar(raw, Some("18.9"), None)).unwrap();
        assert_eq!(weather.temperature_value, Some(18.9));
        assert_eq!(weather.dewpoint_value, None);
        assert!(raw_temperatures(raw).unwrap().dewpoint.is_none());
    }
    let weather = CurrentWeather::try_from(metar(KPWM, None, None)).unwrap();
    assert_eq!(weather.temperature_value, None);
    assert_eq!(weather.dewpoint_value, None);

    // Without raw evidence the guard cannot prove the decoder was wrong.
    let raw = "METAR KPWM 241851Z 06003KT 10SM CLR A3044 RMK AO2";
    let weather = CurrentWeather::try_from(metar(raw, Some("60"), Some("3"))).unwrap();
    assert_eq!(weather.temperature_value, Some(60.0));
    assert_eq!(weather.dewpoint_value, Some(3.0));
    assert_eq!(weather.quality_status, "unverified");
}

#[test]
fn wind_visibility_and_malformed_tokens_are_never_temperature_evidence() {
    for token in [
        "060/03",
        "6/03",
        "19/006",
        "19/M006",
        "R01/0600",
        "3/4SM",
        "19/06X",
        "M19/06/",
        "//",
        "T01890056",
    ] {
        let raw = format!("METAR KPWM 241851Z {token} A3044 RMK AO2");
        assert!(raw_temperatures(&raw).is_none(), "{token}");
    }
    for token in ["T20190056", "T0189005", "T01890056X", "T01é9005", "19/06"] {
        let raw = format!("METAR KPWM 241851Z 06003KT A3044 RMK AO2 {token}");
        assert!(raw_temperatures(&raw).is_none(), "{token}");
    }
}

#[test]
fn duplicate_raw_groups_are_ambiguous_and_flagged_unverified() {
    for raw in [
        "METAR KPWM 241851Z 06003KT 19/06 40/30 A3044",
        "METAR KPWM 241851Z 06003KT 19/06 A3044 RMK T01890056 T04000300",
    ] {
        assert!(raw_temperatures(raw).is_none());
        assert_eq!(
            CurrentWeather::try_from(metar(raw, Some("40"), Some("30")))
                .unwrap()
                .quality_status,
            "unverified"
        );
    }
}

#[test]
fn existing_aviation_weather_cache_fixtures_still_convert() {
    let observations: ObservationData =
        parse_xml(include_str!("testdata/metars.cache.xml")).unwrap();
    for report in observations.data.metar {
        let station = report.station_id.clone();
        if station == "KQEW" {
            assert!(
                CurrentWeather::try_from(report).is_err(),
                "invalid latitude sentinel"
            );
        } else {
            assert!(CurrentWeather::try_from(report).is_ok(), "{station}");
        }
    }
}

#[test]
fn nonfinite_and_malformed_numbers_are_rejected_without_raw_temperature_evidence() {
    for value in ["NaN", "inf", "-inf", "bad", ""] {
        let weather =
            CurrentWeather::try_from(metar("METAR KPWM 241851Z", Some(value), None)).unwrap();
        assert_eq!(weather.quality_status, "rejected", "{value}");
        assert_eq!(weather.temperature_value, None);
        assert!(
            weather
                .quality_reason
                .unwrap()
                .contains("invalid temperature")
        );
    }
    let mut report = metar(KPWM, Some("18.9"), Some("5.6"));
    report.wind_speed_kt = Some("broken".into());
    assert_eq!(
        CurrentWeather::try_from(report).unwrap().quality_status,
        "rejected"
    );
}

#[test]
fn invalid_coordinates_cannot_be_published() {
    for (latitude, longitude) in [("NaN", "-70"), ("-99.99", "-99.99"), ("43", "181")] {
        let mut report = metar(KPWM, Some("18.9"), Some("5.6"));
        report.latitude = Some(latitude.into());
        report.longitude = Some(longitude.into());
        assert!(CurrentWeather::try_from(report).is_err());
    }
}

#[test]
fn timestamps_with_offsets_are_normalized_before_writing_z() {
    let mut report = metar(KPWM, Some("18.9"), Some("5.6"));
    report.observation_time = Some("2026-09-24T14:51:00-04:00".into());
    let observation = Observation::try_from(CurrentWeather::try_from(report).unwrap()).unwrap();
    assert_eq!(observation.generated_at, "2026-09-24T18:51:00Z");
}

fn service() -> ObservationService {
    let logger = slog::Logger::root(slog::Discard, slog::o!());
    let limiter = Arc::new(tokio::sync::Mutex::new(crate::RateLimiter::new(
        1,
        std::time::Duration::from_secs(1),
    )));
    ObservationService::new(
        logger.clone(),
        Arc::new(XmlFetcher::new(logger, "audit-test", limiter).unwrap()),
    )
}

fn stations() -> CityWeather {
    let station = crate::WeatherStation {
        station_id: "KPWM".into(),
        station_name: "Portland".into(),
        state: "ME".into(),
        iata_id: "PWM".into(),
        elevation_m: Some(23.0),
        latitude: "43.65".into(),
        longitude: "-70.31".into(),
    };
    CityWeather {
        city_data: [(station.station_id.clone(), station)]
            .into_iter()
            .collect(),
    }
}

fn data(reports: Vec<Metar>) -> ObservationData {
    let mut data: ObservationData = parse_xml(include_str!("testdata/metars.cache.xml")).unwrap();
    data.data.metar = reports;
    data
}

#[test]
fn parquet_keeps_rejected_values_raw_provenance_and_durable_audit() {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observations.parquet");
    let source = "original source document";
    let data = data(vec![
        metar(KPWM, Some("60"), Some("3")),
        metar(
            &KPWM.replace("060/03", "06003KT"),
            Some("18.9"),
            Some("5.6"),
        ),
    ]);
    let report = service()
        .write_observations(&stations(), path.to_str().unwrap(), source, &data)
        .unwrap();
    assert_eq!(report.written, 2);
    assert_eq!(report.rejected, 1);
    let reader = SerializedFileReader::new(File::open(&path).unwrap()).unwrap();
    let rows = reader
        .get_row_iter(None)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    use parquet::record::Field;
    let field = |name: &str| {
        rows[0]
            .get_column_iter()
            .find(|(key, _)| *key == name)
            .unwrap()
            .1
            .clone()
    };
    assert_eq!(field("temperature_value"), Field::Double(60.0));
    assert_eq!(field("quality_status"), Field::Str("rejected".into()));
    assert_eq!(field("raw_text"), Field::Str(KPWM.into()));
    assert_eq!(
        field("validation_version"),
        Field::Str(VALIDATION_VERSION.into())
    );
    let sidecar = std::fs::read_to_string(format!("{}.quality.json", path.display())).unwrap();
    let audit: serde_json::Value = serde_json::from_str(&sidecar).unwrap();
    assert_eq!(audit["source_sha256"], sha256_hex(source.as_bytes()));
    assert_eq!(audit["issues"][0]["report"]["temp_c"], "60");
    assert_eq!(audit["issues"][0]["report"]["raw_text"], KPWM);
    let metadata = reader
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .unwrap();
    assert!(
        metadata
            .iter()
            .any(|entry| entry.key == "observation_audit"
                && entry.value.as_deref() == Some(&sidecar))
    );
}

#[test]
fn required_field_failures_save_evidence_and_refuse_partial_publication() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observations.parquet");
    let mut invalid = metar(KPWM, Some("18.9"), Some("5.6"));
    invalid.observation_time = Some("not a date".into());
    let data = data(vec![
        metar(
            &KPWM.replace("060/03", "06003KT"),
            Some("18.9"),
            Some("5.6"),
        ),
        invalid,
    ]);
    assert!(
        service()
            .write_observations(&stations(), path.to_str().unwrap(), "source", &data)
            .is_err()
    );
    assert!(!path.exists());
    let audit: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{}.quality.json", path.display())).unwrap(),
    )
    .unwrap();
    assert_eq!(audit["unrepresentable"], 1);
    assert_eq!(
        audit["issues"][0]["report"]["observation_time"],
        "not a date"
    );
}

#[test]
fn failed_evidence_write_prevents_parquet_publication() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observations.parquet");
    std::fs::create_dir(format!("{}.quality.json", path.display())).unwrap();
    let data = data(vec![metar(KPWM, Some("60"), Some("3"))]);
    assert!(
        service()
            .write_observations(&stations(), path.to_str().unwrap(), "source", &data)
            .is_err()
    );
    assert!(!path.exists());
}

#[test]
fn tenths_are_not_allowed_whole_degree_rounding_tolerance() {
    let weather = CurrentWeather::try_from(metar(KPWM, Some("18.4"), Some("5.6"))).unwrap();
    assert_eq!(weather.quality_status, "rejected");
}

#[test]
fn contradicting_raw_body_and_remark_cannot_be_validated() {
    let raw = "METAR KPWM 241851Z 06003KT 10SM CLR 40/30 A3044 RMK AO2 T01890056";
    let weather = CurrentWeather::try_from(metar(raw, Some("18.9"), Some("5.6"))).unwrap();
    assert_eq!(weather.quality_status, "unverified");
}

#[test]
fn gauge_outage_never_creates_zero_rain_and_sensor_outages_are_flagged() {
    let raw = format!("{KPWM} PNO");
    let weather = CurrentWeather::try_from(metar(&raw, Some("18.9"), Some("5.6"))).unwrap();
    assert_eq!(weather.precip_in, None);
    assert_eq!(weather.quality_status, "rejected");
    assert!(weather.quality_reason.unwrap().contains("PNO"));
    let raw = format!("{KPWM} PWINO");
    let weather = CurrentWeather::try_from(metar(&raw, Some("18.9"), Some("5.6"))).unwrap();
    assert_eq!(weather.quality_status, "rejected");
}

#[test]
fn station_and_report_time_disagreements_are_preserved_as_rejected() {
    for raw in [
        KPWM.replace("KPWM", "KJFK"),
        KPWM.replace("241851Z", "241951Z"),
    ] {
        let weather = CurrentWeather::try_from(metar(&raw, Some("18.9"), Some("5.6"))).unwrap();
        assert_eq!(weather.quality_status, "rejected");
        assert!(
            weather
                .quality_reason
                .unwrap()
                .contains("differs from decoded")
        );
    }
}

#[test]
fn raw_precipitation_disagreement_missing_decodes_and_unavailable_gauge_are_flagged() {
    for (group, decoded, accepted) in [
        ("P0012", Some("0.12"), true),
        ("P0012", Some("12"), false),
        ("P0012", None, false),
        ("P////", Some("655.35"), false),
        ("P////", None, false),
        ("P0000", Some("0.005"), true),
        ("P0000", Some("0"), true),
        ("P0000", Some("0.01"), false),
        ("P0012 P0012", Some("0.12"), false),
    ] {
        let raw = format!("{} {group}", KPWM.replace("060/03", "06003KT"));
        let mut report = metar(&raw, Some("18.9"), Some("5.6"));
        report.precip_in = decoded.map(str::to_owned);
        let weather = CurrentWeather::try_from(report).unwrap();
        assert_eq!(
            weather.quality_status == "validated",
            accepted,
            "{group} {decoded:?}"
        );
        if decoded.is_none() {
            assert_eq!(
                weather.precip_in, None,
                "missing decode must not become zero"
            );
        }
    }
}

#[test]
fn saved_awc_source_documents_expose_the_confirmed_decoder_failures() {
    for (xml, station, bad_temperature) in [
        (
            include_str!("testdata/kpwm-decoded-mismatch.xml"),
            "KPWM",
            "60",
        ),
        (
            include_str!("testdata/kapg-decoded-mismatch.xml"),
            "KAPG",
            "-19",
        ),
        (
            include_str!("testdata/ktah-decoded-mismatch.xml"),
            "KTAH",
            "-29",
        ),
        (
            include_str!("testdata/kmcn-decoded-mismatch.xml"),
            "KMCN",
            "0",
        ),
        (
            include_str!("testdata/kpub-decoded-mismatch.xml"),
            "KPUB",
            "0",
        ),
    ] {
        let data: ObservationData = parse_xml(xml).unwrap();
        let report = data
            .data
            .metar
            .into_iter()
            .find(|report| {
                report.station_id == station && report.temp_c.as_deref() == Some(bad_temperature)
            })
            .unwrap();
        let weather = CurrentWeather::try_from(report).unwrap();
        assert_ne!(
            weather.quality_status, "validated",
            "{station}: {}",
            weather.raw_text
        );
        assert!(weather.quality_reason.is_some());
    }
}

#[test]
fn original_metar_type_is_preserved_and_conflicting_type_is_rejected() {
    let raw = "SPECI KPWM 241851Z 06003KT 10SM CLR 19/06 A3044 RMK AO2 T01890056";
    let mut report = metar(raw, Some("18.9"), Some("5.6"));
    report.metar_type = Some("SPECI".into());
    let weather = CurrentWeather::try_from(report.clone()).unwrap();
    assert_eq!(weather.metar_type.as_deref(), Some("SPECI"));
    assert_eq!(weather.quality_status, "validated");
    report.metar_type = Some("METAR".into());
    assert_eq!(
        CurrentWeather::try_from(report).unwrap().quality_status,
        "rejected"
    );
}

#[test]
fn speci_without_hourly_precipitation_group_never_becomes_measured_zero() {
    for raw in [
        "SPECI KPWM 241851Z 06003KT 10SM CLR 19/06 A3044 RMK AO2 T01890056",
        "KPWM 241851Z 06003KT 10SM CLR 19/06 A3044 RMK AO2 T01890056",
    ] {
        for decoded in [None, Some("0")] {
            let mut report = metar(raw, Some("18.9"), Some("5.6"));
            report.metar_type = Some("SPECI".into());
            report.precip_in = decoded.map(str::to_owned);
            let weather = CurrentWeather::try_from(report).unwrap();
            assert_eq!(weather.precip_in, None);
            assert_eq!(weather.quality_status, "validated");
        }
        let mut report = metar(raw, Some("18.9"), Some("5.6"));
        report.metar_type = Some("SPECI".into());
        report.precip_in = Some("0.12".into());
        let weather = CurrentWeather::try_from(report).unwrap();
        assert_eq!(
            weather.precip_in,
            Some(0.12),
            "retain contradictory decoded evidence"
        );
        assert_eq!(weather.quality_status, "rejected");
        assert!(
            weather
                .quality_reason
                .unwrap()
                .contains("no raw hourly P group")
        );
        let mut report = metar(&format!("{raw} P0012"), Some("18.9"), Some("5.6"));
        report.metar_type = Some("SPECI".into());
        report.precip_in = Some("0.12".into());
        let weather = CurrentWeather::try_from(report).unwrap();
        assert_eq!(weather.precip_in, Some(0.12));
        assert_eq!(weather.quality_status, "validated");
    }
}
