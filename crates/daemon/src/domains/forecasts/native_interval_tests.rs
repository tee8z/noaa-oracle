use super::*;
use crate::{Time, parse_xml};
use parquet::file::properties::WriterProperties;
use parquet::file::reader::{FileReader, SerializedFileReader};
use std::fs::File;
use time::macros::datetime;
use tokio::io::AsyncReadExt;

fn fixture() -> Dwml {
    let mut document: Dwml = parse_xml(include_str!("testdata/dwml.xml")).unwrap();
    for (index, location) in document.data.location.iter_mut().enumerate() {
        location.station_id = Some(format!("KT{index:02}"));
    }
    document
}

fn rows(document: Dwml) -> HashMap<String, Vec<WeatherForecast>> {
    document.try_into().unwrap()
}

#[test]
fn native_rows_never_copy_a_metric_onto_another_source_interval() {
    let data = rows(fixture());
    assert_eq!(data.len(), 2);
    for station in data.values() {
        for row in station {
            assert!(!row.provenance.layouts.is_empty());
            for source in row.provenance.layouts.values() {
                let start = OffsetDateTime::parse(&source.start, &Rfc3339).unwrap();
                let end = source
                    .end
                    .as_deref()
                    .map(|value| OffsetDateTime::parse(value, &Rfc3339).unwrap());
                assert_eq!(row.begin_time, start);
                assert_eq!(row.end_time, end.unwrap_or(start));
            }
        }
    }
}

#[test]
fn audited_kc07_humidity_values_keep_their_separate_native_periods() {
    // Captured public NOAA DWML: creation 2026-09-27T13:38:29Z.
    // The previous any-overlap mapping put max83 and min90 into one noon row.
    let mut document: Dwml =
        parse_xml(include_str!("testdata/forecast-humidity-kc07.xml")).unwrap();
    for location in &mut document.data.location {
        location.station_id = Some(location.location_key.clone());
    }
    let data = rows(document);
    let station = data
        .values()
        .find(|station| {
            station.iter().any(|row| {
                row.begin_time == datetime!(2026-09-28 06:00 UTC)
                    && row.relative_humidity_max == Some(83)
            })
        })
        .expect("audited station");
    let maximum = station
        .iter()
        .find(|row| {
            row.begin_time == datetime!(2026-09-28 06:00 UTC)
                && row.relative_humidity_max == Some(83)
        })
        .unwrap();
    assert_eq!(maximum.end_time, datetime!(2026-09-28 18:00 UTC));
    assert_eq!(maximum.relative_humidity_min, None);
    let minimum = station
        .iter()
        .find(|row| {
            row.begin_time == datetime!(2026-09-28 18:00 UTC)
                && row.relative_humidity_min == Some(90)
        })
        .unwrap();
    assert_eq!(minimum.end_time, datetime!(2026-09-29 06:00 UTC));
    assert_eq!(minimum.relative_humidity_max, None);
    assert!(!station.iter().any(|row| {
        row.relative_humidity_min
            .zip(row.relative_humidity_max)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
    }));
}

#[test]
fn wind_points_have_no_invented_duration_even_at_the_last_sample() {
    let source = fixture();
    let expected = source.data.parameters[0]
        .wind_speed
        .as_ref()
        .unwrap()
        .value
        .len();
    let data = rows(source);
    let winds: Vec<_> = data["KT00"]
        .iter()
        .filter(|row| row.provenance.layouts.contains_key("wind_speed"))
        .collect();
    assert_eq!(winds.len(), expected);
    assert!(winds.iter().all(|row| row.begin_time == row.end_time));
    let last = Forecast::try_from((**winds.last().unwrap()).clone()).unwrap();
    assert_eq!(last.interval_kind.as_deref(), Some("instant"));
    assert!(
        winds.last().unwrap().provenance.layouts["wind_speed"]
            .end
            .is_none()
    );
}

#[test]
fn every_precipitation_amount_is_written_once_on_its_original_interval() {
    let source = fixture();
    let precipitation = source.data.parameters[0]
        .precipitation
        .as_ref()
        .unwrap()
        .iter()
        .find(|value| value.reading_type == Liquid)
        .unwrap();
    let expected_count = precipitation.value.len();
    let expected_total: f64 = precipitation
        .value
        .iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .sum();
    let data = rows(source);
    let liquid: Vec<_> = data["KT00"]
        .iter()
        .filter(|row| {
            row.provenance
                .layouts
                .contains_key("liquid_precipitation_amt")
        })
        .collect();
    assert_eq!(liquid.len(), expected_count);
    assert!(
        liquid
            .iter()
            .all(|row| row.end_time - row.begin_time == Duration::hours(6))
    );
    assert!(
        (liquid
            .iter()
            .filter_map(|row| row.liquid_precipitation_amt)
            .sum::<f64>()
            - expected_total)
            .abs()
            < 1e-9
    );
}

#[test]
fn values_cannot_shift_when_layout_and_value_counts_disagree() {
    let mut document = fixture();
    document.data.parameters[0]
        .wind_speed
        .as_mut()
        .unwrap()
        .value
        .remove(0);
    let result: Result<HashMap<String, Vec<WeatherForecast>>, Error> = document.try_into();
    assert!(result.unwrap_err().to_string().contains("native intervals"));
}

#[test]
fn unknown_units_are_retained_and_rejected_instead_of_assuming_knots() {
    let xml =
        include_str!("testdata/dwml.xml").replace("units=\"knots\"", "units=\"mystery-speed\"");
    let mut source: Dwml = parse_xml(&xml).unwrap();
    source.data.location[0].station_id = Some("KUNK".into());
    let data = rows(source);
    let row = data["KUNK"]
        .iter()
        .find(|row| row.wind_speed.is_some())
        .unwrap();
    let written = Forecast::try_from(row.clone()).unwrap();
    assert_eq!(written.quality_status.as_deref(), Some("rejected"));
    assert_eq!(written.wind_speed_unit_code, "mystery-speed");
    assert!(
        written
            .quality_reason
            .unwrap()
            .contains("unsupported wind_speed units")
    );
}

#[test]
fn nonfinite_negative_and_fractional_wind_values_are_quarantined() {
    for bad in ["NaN", "-1", "1.5", "broken"] {
        let mut source = fixture();
        source.data.parameters[0].wind_speed.as_mut().unwrap().value[0] = bad.into();
        let data = rows(source);
        let row = data["KT00"]
            .iter()
            .find(|row| {
                row.provenance
                    .layouts
                    .get("wind_speed")
                    .is_some_and(|source| source.index == 0)
            })
            .unwrap();
        let written = Forecast::try_from(row.clone()).unwrap();
        assert_eq!(written.quality_status.as_deref(), Some("rejected"), "{bad}");
        assert_eq!(written.wind_speed, None);
        assert!(written.source_layouts.unwrap().contains(bad));
    }
}

#[test]
fn nil_values_stay_missing_without_invalidating_other_valid_native_metrics() {
    let mut source = fixture();
    source.data.parameters[0].wind_speed.as_mut().unwrap().value[0].clear();
    let data = rows(source);
    let row = data["KT00"]
        .iter()
        .find(|row| {
            row.provenance
                .layouts
                .get("wind_speed")
                .is_some_and(|source| source.index == 0)
        })
        .unwrap();
    assert_eq!(row.wind_speed, None);
    assert!(row.provenance.problems.is_empty());
    assert!(row.provenance.layouts["wind_speed"].value.is_empty());
}

#[test]
fn accumulation_without_a_source_end_is_rejected_not_interpolated() {
    let mut source = fixture();
    let key = source.data.parameters[0]
        .precipitation
        .as_ref()
        .unwrap()
        .iter()
        .find(|value| value.reading_type == Liquid)
        .unwrap()
        .time_layout
        .clone();
    let layout = source
        .data
        .time_layout
        .iter_mut()
        .find(|layout| {
            layout
                .time
                .iter()
                .any(|time| matches!(time, Time::LayoutKey(value) if value == &key))
        })
        .unwrap();
    layout.time.retain(|time| !matches!(time, Time::EndTime(_)));
    let data = rows(source);
    for row in data["KT00"].iter().filter(|row| {
        row.provenance
            .layouts
            .contains_key("liquid_precipitation_amt")
    }) {
        assert_eq!(row.begin_time, row.end_time);
        assert!(
            row.provenance
                .problems
                .iter()
                .any(|reason| reason.contains("no explicit source interval"))
        );
    }
}

#[tokio::test]
async fn published_parquet_retains_native_schema_and_exact_source_xml() {
    let xml = include_str!("testdata/dwml.xml");
    let hash = Sha256::digest(xml.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let source = forecast_source_document(
        xml,
        "https://example.test/ndfd",
        "2026-09-27T13:00:00Z",
        &hash,
    )
    .await
    .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(source.value.as_ref().unwrap()).unwrap();
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(metadata["content"].as_str().unwrap())
        .unwrap();
    let mut decoder = async_compression::tokio::bufread::GzipDecoder::new(compressed.as_slice());
    let mut restored = String::new();
    decoder.read_to_string(&mut restored).await.unwrap();
    assert_eq!(restored, xml);
    let data = rows(fixture());
    let forecasts: Vec<Forecast> = data["KT00"]
        .iter()
        .cloned()
        .map(|row| row.try_into().unwrap())
        .collect();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("forecasts.parquet");
    let file = File::create(&path).unwrap();
    let mut writer = SerializedFileWriter::new(
        file,
        Arc::new(create_forecast_schema()),
        Arc::new(WriterProperties::builder().build()),
    )
    .unwrap();
    writer.append_key_value_metadata(source);
    let mut group = writer.next_row_group().unwrap();
    forecasts.as_slice().write_to_row_group(&mut group).unwrap();
    group.close().unwrap();
    writer.close().unwrap();
    let reader = SerializedFileReader::new(File::open(path).unwrap()).unwrap();
    assert_eq!(
        reader.metadata().file_metadata().num_rows(),
        forecasts.len() as i64
    );
    assert!(
        reader
            .metadata()
            .file_metadata()
            .schema_descr()
            .columns()
            .iter()
            .any(|column| column.name() == "source_layouts")
    );
    assert_eq!(
        reader
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .unwrap()[0]
            .key,
        format!("noaa_forecast_source:{hash}")
    );
}

#[test]
fn captured_kmci_rain_forecast_has_measured_native_components_for_scoring() {
    // Official DWML retained during the audit for 39.2976,-94.7139 with qpf,
    // snow and iceaccum enabled. Positive rain does not need an invented ratio
    // when the same native period explicitly forecasts zero snow and ice.
    let mut document: Dwml = parse_xml(include_str!("testdata/forecast-rain-kmci.xml")).unwrap();
    for location in &mut document.data.location {
        location.station_id = Some("KMCI".into());
    }
    let data = rows(document);
    let positive: Vec<_> = data["KMCI"]
        .iter()
        .filter(|row| {
            row.liquid_precipitation_amt
                .is_some_and(|value| value > 0.0)
        })
        .collect();
    assert!(!positive.is_empty());
    for row in positive {
        assert_eq!(row.end_time - row.begin_time, Duration::hours(6));
        assert_eq!(row.snow_amt, Some(0.0));
        assert_eq!(row.ice_amt, Some(0.0));
        assert!(
            row.provenance.problems.is_empty(),
            "{:?}",
            row.provenance.problems
        );
        for metric in ["liquid_precipitation_amt", "snow_amt", "ice_amt"] {
            let source = &row.provenance.layouts[metric];
            assert_eq!(
                OffsetDateTime::parse(&source.start, &Rfc3339).unwrap(),
                row.begin_time
            );
            assert_eq!(
                OffsetDateTime::parse(source.end.as_deref().unwrap(), &Rfc3339).unwrap(),
                row.end_time
            );
        }
    }
}
