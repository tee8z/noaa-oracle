use super::*;
use serde_json::json;
use std::io::Write;
use time::macros::datetime;

fn start() -> i64 {
    micros(datetime!(2026-01-17 00:00 UTC))
}
const HOUR: i64 = 3_600_000_000;

fn timestamp(value: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(value) * 1_000)
        .unwrap()
        .format(&Rfc3339)
        .unwrap()
}

fn document() -> SourceDocument {
    let bytes = b"<dwml><data>retained source fixture</data></dwml>";
    let hash = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut encoder = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    SourceDocument {
        url: format!("{SOURCE_PREFIX}listLatLon=43.65,-70.31&product=time-series"),
        received_at: "2026-01-16T23:00:00Z".into(),
        sha256: hash,
        encoding: "gzip+base64".into(),
        content: base64::engine::general_purpose::STANDARD.encode(encoder.finish().unwrap()),
    }
}

fn row(begin: i64, finish: i64, values: &[(&str, Option<f64>)]) -> Row {
    let source = document();
    let mut layouts = serde_json::Map::new();
    for (metric, value) in values {
        let units = match *metric {
            "min_temp" | "max_temp" => "fahrenheit",
            "wind_speed" => "knots",
            "wind_direction" => "degrees true",
            "relative_humidity_max" | "relative_humidity_min" | "snow_ratio" => "percent",
            _ => "inches",
        };
        layouts.insert(
            (*metric).into(),
            json!({"layout":"k-fixture", "start":timestamp(begin),
            "end":(finish != begin).then(|| timestamp(finish)), "index":0,
            "value":value.map(|value| value.to_string()).unwrap_or_default(), "units":units}),
        );
    }
    Row {
        station_id: "KPWM".into(),
        begin: Some(begin),
        end: Some(finish),
        generated: Some(start() - 2 * HOUR),
        published: Some(start() - HOUR),
        source: "2026-01-16/forecasts_2026-01-16T23:00:00Z.parquet".into(),
        values: values
            .iter()
            .map(|(key, value)| ((*key).into(), *value))
            .collect(),
        forecast_interval_version: Some(VERSION.into()),
        interval_kind: Some(if begin == finish { "instant" } else { "period" }.into()),
        source_url: Some(source.url),
        source_received_at: Some(source.received_at),
        source_xml_sha256: Some(source.sha256),
        source_location: Some("point1".into()),
        source_layouts: Some(serde_json::Value::Object(layouts).to_string()),
        quality_status: Some("validated".into()),
        quality_reason: None,
    }
}

fn documents(rows: &[Row]) -> Documents {
    rows.iter()
        .map(|row| {
            let mut document = document();
            document.received_at = row.source_received_at.clone().unwrap_or_default();
            ((row.source.clone(), document.sha256.clone()), document)
        })
        .collect()
}

fn result(rows: &[Row], metric: &str, end: i64) -> Option<f64> {
    evaluate(rows, &documents(rows), &["KPWM".into()], start(), end)
        .into_iter()
        .find(|value| value.metric == metric)
        .unwrap()
        .value
}

fn rain_row(begin: i64, finish: i64, amount: f64) -> Row {
    row(
        begin,
        finish,
        &[
            ("liquid_precipitation_amt", Some(amount)),
            ("snow_amt", Some(0.0)),
            ("ice_amt", Some(0.0)),
        ],
    )
}

#[test]
fn precipitation_sums_the_periods_centred_in_the_window_without_gaps() {
    let complete = [
        rain_row(start(), start() + 6 * HOUR, 0.25),
        rain_row(start() + 6 * HOUR, start() + 12 * HOUR, 0.5),
    ];
    assert_eq!(
        result(&complete, "rain_amt", start() + 12 * HOUR),
        Some(0.75)
    );
    assert_eq!(
        result(&complete, "snow_amt", start() + 12 * HOUR),
        Some(0.0)
    );
    assert_eq!(
        result(&complete, "rain_amt", start() + 11 * HOUR),
        Some(0.75),
        "a period counts whole in the window that holds its midpoint"
    );
    assert_eq!(
        result(&complete, "rain_amt", start() + 8 * HOUR),
        Some(0.25),
        "the second period is centred in the next window"
    );
    assert_eq!(
        result(&complete[..1], "rain_amt", start() + 12 * HOUR),
        None,
        "missing final period"
    );
    let gap = [
        complete[0].clone(),
        rain_row(start() + 7 * HOUR, start() + 12 * HOUR, 0.5),
    ];
    assert_eq!(result(&gap, "rain_amt", start() + 12 * HOUR), None);
    let overlap = [
        complete[0].clone(),
        rain_row(start() + 5 * HOUR, start() + 12 * HOUR, 0.5),
    ];
    assert_eq!(result(&overlap, "rain_amt", start() + 12 * HOUR), None);
}

#[test]
fn missing_precipitation_phase_is_not_zero_and_ice_is_not_liquid_depth() {
    for values in [
        vec![
            ("liquid_precipitation_amt", Some(0.5)),
            ("snow_amt", Some(0.0)),
        ],
        vec![
            ("liquid_precipitation_amt", Some(0.5)),
            ("snow_amt", None),
            ("ice_amt", Some(0.0)),
        ],
        vec![
            ("liquid_precipitation_amt", Some(0.5)),
            ("snow_amt", Some(0.0)),
            ("ice_amt", Some(0.1)),
        ],
    ] {
        assert_eq!(
            result(
                &[row(start(), start() + 6 * HOUR, &values)],
                "rain_amt",
                start() + 6 * HOUR
            ),
            None
        );
    }
}

#[test]
fn nonzero_snow_requires_a_ratio_for_its_exact_native_interval() {
    let snow = row(
        start(),
        start() + 6 * HOUR,
        &[
            ("liquid_precipitation_amt", Some(0.5)),
            ("snow_amt", Some(2.0)),
            ("ice_amt", Some(0.0)),
        ],
    );
    assert_eq!(
        result(std::slice::from_ref(&snow), "snow_amt", start() + 6 * HOUR),
        Some(2.0)
    );
    assert_eq!(
        result(std::slice::from_ref(&snow), "rain_amt", start() + 6 * HOUR),
        None
    );
    let ratio = row(start(), start() + 6 * HOUR, &[("snow_ratio", Some(10.0))]);
    assert!(
        (result(&[snow.clone(), ratio], "rain_amt", start() + 6 * HOUR).unwrap() - 0.3).abs()
            < 1e-9
    );
    let other_period = row(start(), start() + 3 * HOUR, &[("snow_ratio", Some(10.0))]);
    assert_eq!(
        result(&[snow, other_period], "rain_amt", start() + 6 * HOUR),
        None
    );
}

#[test]
fn latest_issue_nil_or_removed_metric_does_not_fall_back_to_older_values() {
    let old = row(start(), start() + 12 * HOUR, &[("max_temp", Some(60.0))]);
    let mut new = old.clone();
    new.generated = Some(start() - HOUR);
    new.values.insert("max_temp".into(), None);
    assert_eq!(
        result(&[old.clone(), new], "temp_high", start() + 12 * HOUR),
        None
    );
    let mut removed = row(start(), start(), &[("wind_speed", Some(10.0))]);
    removed.generated = Some(start() - HOUR);
    assert_eq!(
        result(&[old, removed], "temp_high", start() + 12 * HOUR),
        None
    );
}

#[test]
fn refetch_after_event_start_cannot_hide_the_pre_event_publication() {
    let before = row(start(), start(), &[("wind_speed", Some(12.0))]);
    let mut after = before.clone();
    after.published = Some(start() + HOUR);
    after.source = "2026-01-17/forecasts_2026-01-17T01:00:00Z.parquet".into();
    after.source_received_at = Some(timestamp(start() + HOUR));
    after.values.insert("wind_speed".into(), Some(99.0));
    let tail = row(
        start() + 6 * HOUR,
        start() + 6 * HOUR,
        &[("wind_speed", Some(1.0))],
    );
    assert_eq!(
        result(
            &[before, tail, after.clone()],
            "wind_speed",
            start() + 6 * HOUR
        ),
        Some(12.0)
    );
    assert_eq!(result(&[after], "wind_speed", start() + 6 * HOUR), None);
}

#[test]
fn duplicate_conflicting_values_are_unavailable_but_identical_copies_are_safe() {
    let first = row(start(), start(), &[("wind_speed", Some(12.0))]);
    let tail = row(
        start() + 6 * HOUR,
        start() + 6 * HOUR,
        &[("wind_speed", Some(1.0))],
    );
    assert_eq!(
        result(
            &[first.clone(), first.clone(), tail.clone()],
            "wind_speed",
            start() + 6 * HOUR
        ),
        Some(12.0)
    );
    let second = row(start(), start(), &[("wind_speed", Some(15.0))]);
    assert_eq!(
        result(&[first, second, tail], "wind_speed", start() + 6 * HOUR),
        None
    );
}

#[test]
fn native_extrema_stay_inside_window_and_opposite_half_day_gaps_are_allowed() {
    let high = row(
        start() + 6 * HOUR,
        start() + 18 * HOUR,
        &[("max_temp", Some(65.0))],
    );
    let next = row(
        start() + 30 * HOUR,
        start() + 42 * HOUR,
        &[("max_temp", Some(67.0))],
    );
    assert_eq!(
        result(&[high.clone(), next], "temp_high", start() + 48 * HOUR),
        Some(67.0)
    );
    assert_eq!(
        result(
            std::slice::from_ref(&high),
            "temp_high",
            start() + 48 * HOUR
        ),
        None,
        "whole missing day"
    );
    assert_eq!(
        result(
            std::slice::from_ref(&high),
            "temp_high",
            start() + 36 * HOUR
        ),
        Some(65.0),
        "the next day's high is centred at the window's end, in the next window"
    );
    assert_eq!(
        result(
            std::slice::from_ref(&high),
            "temp_high",
            start() + 40 * HOUR
        ),
        None,
        "the next day's high is centred in the window but missing"
    );
    assert_eq!(
        result(
            std::slice::from_ref(&high),
            "temp_high",
            start() + 12 * HOUR
        ),
        None,
        "a period centred at the window's end belongs to the next window"
    );
    assert_eq!(
        result(
            std::slice::from_ref(&high),
            "temp_high",
            start() + 13 * HOUR
        ),
        Some(65.0),
        "a period counts in the window that holds its midpoint"
    );
    assert_eq!(
        result(&[high], "temp_high", start() + 24 * HOUR),
        Some(65.0)
    );
}

#[test]
fn wind_only_window_needs_no_temperature_and_direction_is_at_the_peak() {
    let samples = [
        row(
            start() - HOUR,
            start() - HOUR,
            &[("wind_speed", Some(99.0)), ("wind_direction", Some(300.0))],
        ),
        row(
            start(),
            start(),
            &[("wind_speed", Some(12.0)), ("wind_direction", Some(350.0))],
        ),
        row(
            start() + HOUR,
            start() + HOUR,
            &[("wind_speed", Some(20.0)), ("wind_direction", Some(10.0))],
        ),
        row(
            start() + 6 * HOUR,
            start() + 6 * HOUR,
            &[("wind_speed", Some(99.0)), ("wind_direction", Some(300.0))],
        ),
    ];
    assert_eq!(
        result(&samples, "wind_speed", start() + 6 * HOUR),
        Some(20.0)
    );
    assert_eq!(
        result(&samples, "wind_direction", start() + 6 * HOUR),
        Some(10.0)
    );
    assert_eq!(result(&samples, "temp_high", start() + 6 * HOUR), None);
}

#[test]
fn row_status_units_source_value_and_layout_must_all_match() {
    let valid = row(start(), start(), &[("wind_speed", Some(12.0))]);
    let mut status = valid.clone();
    status.quality_status = Some("rejected".into());
    let mut legacy = valid.clone();
    legacy.forecast_interval_version = None;
    let mut edited = valid.clone();
    edited.values.insert("wind_speed".into(), Some(13.0));
    let mut units = valid.clone();
    units.source_layouts = units
        .source_layouts
        .map(|text| text.replace("knots", "mph"));
    let mut shifted = valid.clone();
    shifted.begin = Some(start() + HOUR);
    shifted.end = shifted.begin;
    for invalid in [status, legacy, edited, units, shifted] {
        assert_eq!(result(&[invalid], "wind_speed", start() + 6 * HOUR), None);
    }
    assert_eq!(
        evaluate(
            &[valid],
            &Documents::new(),
            &["KPWM".into()],
            start(),
            start() + 6 * HOUR
        )[2]
        .value,
        None
    );
}

#[test]
fn retained_source_hash_and_compression_are_verified() {
    let valid = document();
    assert!(document_valid(&valid, &valid.sha256));
    let mut corrupt = valid.clone();
    corrupt.sha256 = "a".repeat(64);
    assert!(!document_valid(&corrupt, &corrupt.sha256));
    corrupt = valid.clone();
    corrupt.content = "not gzip".into();
    assert!(!document_valid(&corrupt, &corrupt.sha256));
    corrupt = valid;
    corrupt.url = "https://untrusted.example/data".into();
    assert!(!document_valid(&corrupt, &corrupt.sha256));
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn write_parquet(directory: &tempfile::TempDir, rows: &[Row], retain_source: bool) {
    let source = &rows[0].source;
    let path = directory.path().join(source);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let selects: Vec<_> = rows
        .iter()
        .map(|row| {
            let text =
                |value: &Option<String>| value.as_deref().map(sql_string).unwrap_or("NULL".into());
            let mut columns = vec![
                format!("{} AS station_id", sql_string(&row.station_id)),
                format!(
                    "{} AS begin_time",
                    sql_string(&timestamp(row.begin.unwrap()))
                ),
                format!("{} AS end_time", sql_string(&timestamp(row.end.unwrap()))),
                format!(
                    "{} AS generated_at",
                    row.generated
                        .map(timestamp)
                        .as_deref()
                        .map(sql_string)
                        .unwrap_or("NULL".into())
                ),
                "'fahrenheit' AS temperature_unit_code".into(),
            ];
            for name in [
                "min_temp",
                "max_temp",
                "wind_speed",
                "wind_direction",
                "relative_humidity_max",
                "relative_humidity_min",
                "liquid_precipitation_amt",
                "snow_amt",
                "snow_ratio",
                "ice_amt",
            ] {
                let value = row
                    .values
                    .get(name)
                    .copied()
                    .flatten()
                    .map(|value| value.to_string())
                    .unwrap_or("NULL".into());
                columns.push(format!("{value}::DOUBLE AS {name}"));
            }
            for (name, value) in [
                ("forecast_interval_version", &row.forecast_interval_version),
                ("interval_kind", &row.interval_kind),
                ("source_url", &row.source_url),
                ("source_received_at", &row.source_received_at),
                ("source_xml_sha256", &row.source_xml_sha256),
                ("source_location", &row.source_location),
                ("source_layouts", &row.source_layouts),
                ("quality_status", &row.quality_status),
                ("quality_reason", &row.quality_reason),
            ] {
                columns.push(format!("{}::VARCHAR AS {name}", text(value)));
            }
            format!("SELECT {}", columns.join(","))
        })
        .collect();
    let document = document();
    let metadata = if retain_source {
        format!(", KV_METADATA {{{}: {}}}",sql_string(&format!("noaa_forecast_source:{}",document.sha256)),
            sql_string(&json!({"url":document.url,"received_at":document.received_at,"sha256":document.sha256,"encoding":document.encoding,"content":document.content}).to_string()))
    } else {
        String::new()
    };
    open_connection()
        .unwrap()
        .execute_batch(&format!(
            "COPY ({}) TO {} (FORMAT PARQUET{metadata})",
            selects.join(" UNION ALL "),
            sql_string(&path.to_string_lossy())
        ))
        .unwrap();
}

#[tokio::test]
async fn strict_query_reads_original_native_rows_and_retained_footer_without_temperature() {
    let directory = tempfile::tempdir().unwrap();
    let rows = [
        row(
            start(),
            start(),
            &[("wind_speed", Some(12.0)), ("wind_direction", Some(270.0))],
        ),
        rain_row(start(), start() + 6 * HOUR, 0.25),
        row(
            start() + 6 * HOUR,
            start() + 6 * HOUR,
            &[("wind_speed", Some(1.0)), ("wind_direction", Some(270.0))],
        ),
    ];
    write_parquet(&directory, &rows, true);
    let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
        directory.path().to_string_lossy().into_owned(),
    )));
    let request = ForecastRequest {
        start: Some(datetime!(2026-01-17 00:00 UTC)),
        end: Some(datetime!(2026-01-17 06:00 UTC)),
        generated_start: Some(datetime!(2026-01-16 00:00 UTC)),
        generated_end: Some(datetime!(2026-01-16 23:59:59 UTC)),
        station_ids: "KPWM".into(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let result = access
        .settlement_forecasts(&request, vec!["KPWM".into()])
        .await
        .unwrap();
    assert_eq!(result.len(), 7);
    assert_eq!(
        result
            .iter()
            .find(|value| value.metric == "wind_speed")
            .unwrap()
            .value,
        Some(12.0)
    );
    assert_eq!(
        result
            .iter()
            .find(|value| value.metric == "temp_high")
            .unwrap()
            .value,
        None
    );
    assert_eq!(
        result
            .iter()
            .find(|value| value.metric == "rain_amt")
            .unwrap()
            .value,
        Some(0.25)
    );
    assert_eq!(
        result
            .iter()
            .find(|value| value.metric == "snow_amt")
            .unwrap()
            .value,
        Some(0.0)
    );
    let assessments = access
        .forecast_assessment(&request, vec!["KPWM".into()])
        .await
        .unwrap();
    for assessment in &assessments {
        assert_eq!(
            assessment.value,
            result
                .iter()
                .find(|value| value.metric == assessment.metric)
                .unwrap()
                .value
        );
        assert_eq!(assessment.reason.is_none(), assessment.value.is_some());
    }
    let rain = assessments
        .iter()
        .find(|value| value.metric == "rain_amt")
        .unwrap();
    assert!(
        rain.native_intervals
            .iter()
            .any(|period| period.metric == "liquid_precipitation_amt"
                && parsed(&period.start) == Some(start())
                && period.end.as_deref().and_then(parsed) == Some(start() + 6 * HOUR))
    );
}

#[tokio::test]
async fn native_marker_without_retained_source_cannot_authorize_settlement() {
    let directory = tempfile::tempdir().unwrap();
    write_parquet(
        &directory,
        &[rain_row(start(), start() + 6 * HOUR, 0.25)],
        false,
    );
    let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
        directory.path().to_string_lossy().into_owned(),
    )));
    let request = ForecastRequest {
        start: Some(datetime!(2026-01-17 00:00 UTC)),
        end: Some(datetime!(2026-01-17 06:00 UTC)),
        generated_start: None,
        generated_end: None,
        station_ids: "KPWM".into(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let result = access
        .settlement_forecasts(&request, vec!["KPWM".into()])
        .await
        .unwrap();
    assert!(result.iter().all(|value| value.value.is_none()));
    let assessments = access
        .forecast_assessment(&request, vec!["KPWM".into()])
        .await
        .unwrap();
    assert!(
        assessments
            .iter()
            .all(|value| value.reason.is_some() && value.native_intervals.is_empty())
    );
}

#[tokio::test]
async fn public_forecasts_hide_rejected_latest_metrics_without_restoring_older_values() {
    let directory = tempfile::tempdir().unwrap();
    let original = [
        row(
            start(),
            start() + 12 * HOUR,
            &[("min_temp", Some(40.0)), ("max_temp", Some(65.0))],
        ),
        row(start(), start(), &[("wind_speed", Some(12.0))]),
        rain_row(start(), start() + 6 * HOUR, 0.25),
    ];
    write_parquet(&directory, &original, false);
    let mut latest = original.to_vec();
    for forecast in &mut latest {
        forecast.source = "2026-01-16/forecasts_2026-01-16T23:30:00Z.parquet".into();
        forecast.published = Some(start() - HOUR / 2);
    }
    for forecast in &mut latest[1..] {
        forecast.quality_status = Some("rejected".into());
        forecast.quality_reason = Some("unsupported source units".into());
    }
    latest[1].values.insert("wind_speed".into(), Some(77.0));
    latest[2]
        .values
        .insert("liquid_precipitation_amt".into(), Some(9.0));
    let mut overlap = row(
        start() - 6 * HOUR,
        start() + 6 * HOUR,
        &[("relative_humidity_min", Some(30.0))],
    );
    overlap.source = latest[0].source.clone();
    overlap.published = latest[0].published;
    overlap.quality_status = Some("rejected".into());
    latest.push(overlap);
    write_parquet(&directory, &latest, false);
    let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
        directory.path().to_string_lossy().into_owned(),
    )));
    let request = ForecastRequest {
        start: Some(datetime!(2026-01-17 00:00 UTC)),
        end: Some(datetime!(2026-01-17 12:00 UTC)),
        generated_start: None,
        generated_end: None,
        station_ids: "KPWM".into(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let forecasts = access
        .forecasts_data(&request, vec!["KPWM".into()])
        .await
        .unwrap();
    assert_eq!(forecasts.len(), 1);
    assert_eq!((forecasts[0].temp_low, forecasts[0].temp_high), (40, 65));
    assert_eq!(
        forecasts[0].wind_speed, None,
        "neither rejected77 nor previous12 may appear"
    );
    assert_eq!(
        forecasts[0].rain_amt, None,
        "neither rejected9 nor previous0.25 may appear"
    );
    let (_, mut quality) = access
        .calendar_forecasts_with_quality(&request, vec!["KPWM".into()], Calendar::Utc)
        .await
        .unwrap();
    assert_eq!(quality.rejected_rows, 3);
    quality.retain_days("2026-01-17", "2026-01-18");
    assert_eq!(
        quality.rejected_rows, 2,
        "hidden previous-day overlap must not be counted twice between panels"
    );
}

#[tokio::test]
async fn audited_kc07_daily_humidity_inversion_is_hidden_with_its_native_periods() {
    let directory = tempfile::tempdir().unwrap();
    // Same 83/90 values and 06–18 / 18–next06 UTC periods as the audited
    // KC07 DWML fixture; use the existing historical test date for stability.
    let mut rows = [
        row(
            start(),
            start() + 24 * HOUR,
            &[("min_temp", Some(40.0)), ("max_temp", Some(65.0))],
        ),
        row(
            start() + 6 * HOUR,
            start() + 18 * HOUR,
            &[("relative_humidity_max", Some(83.0))],
        ),
        row(
            start() + 18 * HOUR,
            start() + 30 * HOUR,
            &[("relative_humidity_min", Some(90.0))],
        ),
    ];
    for row in &mut rows {
        row.station_id = "KC07".into();
    }
    write_parquet(&directory, &rows, true);
    let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
        directory.path().to_string_lossy().into_owned(),
    )));
    let request = ForecastRequest {
        start: Some(datetime!(2026-01-17 00:00 UTC)),
        end: Some(datetime!(2026-01-18 00:00 UTC)),
        generated_start: None,
        generated_end: None,
        station_ids: "KC07".into(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let (forecasts, quality) = access
        .calendar_forecasts_with_quality(&request, vec!["KC07".into()], Calendar::Utc)
        .await
        .unwrap();
    assert_eq!(forecasts.len(), 1);
    assert_eq!(
        (forecasts[0].humidity_min, forecasts[0].humidity_max),
        (None, None)
    );
    assert_eq!(
        (quality.rejected_rows, quality.unverified_rows),
        (0, 0),
        "valid source periods are not mislabeled source corruption"
    );
    assert_eq!(quality.range_issues.len(), 1);
    let issue = &quality.range_issues[0];
    assert_eq!(
        (issue.metric.as_str(), issue.minimum, issue.maximum),
        ("humidity", 90.0, 83.0)
    );
    assert_eq!(issue.native_intervals.len(), 2);
    assert!(
        issue
            .native_intervals
            .iter()
            .any(|period| period.metric == "relative_humidity_min"
                && parsed(&period.start) == Some(start() + 18 * HOUR)
                && period.end.as_deref().and_then(parsed) == Some(start() + 30 * HOUR))
    );
    let html = crate::templates::fragments::forecast_detail(
        "KC07",
        &[],
        &[],
        "UTC",
        &Default::default(),
        &quality,
    )
    .into_string();
    assert!(html.contains("Forecast data warning"));
    assert!(html.contains("daily humidity range hidden"));
    assert!(html.contains("2026-01-17T18:00:00Z to 2026-01-18T06:00:00Z"));
    assert!(!html.contains("90–83%"));
}

#[tokio::test]
async fn inverted_temperature_day_keeps_its_warning_when_no_forecast_dto_can_be_formed() {
    let directory = tempfile::tempdir().unwrap();
    let rows = [
        row(start(), start() + 12 * HOUR, &[("min_temp", Some(70.0))]),
        row(
            start() + 12 * HOUR,
            start() + 24 * HOUR,
            &[("max_temp", Some(60.0))],
        ),
    ];
    write_parquet(&directory, &rows, true);
    let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
        directory.path().to_string_lossy().into_owned(),
    )));
    let request = ForecastRequest {
        start: Some(datetime!(2026-01-17 00:00 UTC)),
        end: Some(datetime!(2026-01-18 00:00 UTC)),
        generated_start: None,
        generated_end: None,
        station_ids: "KPWM".into(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let (forecasts, quality) = access
        .calendar_forecasts_with_quality(&request, vec!["KPWM".into()], Calendar::Utc)
        .await
        .unwrap();
    assert!(forecasts.is_empty());
    assert_eq!(quality.range_issues.len(), 1);
    assert_eq!(quality.range_issues[0].metric, "temperature");
}

#[test]
fn wind_needs_a_grid_spanning_the_event_without_large_holes() {
    let head = row(start(), start(), &[("wind_speed", Some(12.0))]);
    let tail = row(
        start() + 6 * HOUR,
        start() + 6 * HOUR,
        &[("wind_speed", Some(1.0))],
    );
    assert_eq!(
        result(
            &[head.clone(), tail.clone()],
            "wind_speed",
            start() + 6 * HOUR
        ),
        Some(12.0)
    );
    assert_eq!(
        result(
            std::slice::from_ref(&head),
            "wind_speed",
            start() + 6 * HOUR
        ),
        None
    );
    assert_eq!(result(&[tail], "wind_speed", start() + 6 * HOUR), None);
    let distant = row(
        start() + 24 * HOUR,
        start() + 24 * HOUR,
        &[("wind_speed", Some(1.0))],
    );
    assert_eq!(
        result(&[head, distant], "wind_speed", start() + 24 * HOUR),
        None
    );
}

#[test]
fn latest_publication_without_issue_timestamp_cannot_resurrect_older_issue() {
    let old = rain_row(start(), start() + 6 * HOUR, 0.25);
    let mut incomplete = old.clone();
    incomplete.generated = None;
    incomplete.published = Some(start() - HOUR / 2);
    incomplete.source = "2026-01-16/forecasts_2026-01-16T23:30:00Z.parquet".into();
    assert_eq!(
        result(
            &[old.clone(), incomplete.clone()],
            "rain_amt",
            start() + 6 * HOUR
        ),
        None
    );
    // A complete later publication can clear the missing issue metadata.
    let mut repaired = old;
    repaired.published = Some(start() - HOUR / 4);
    repaired.source = "2026-01-16/forecasts_2026-01-16T23:45:00Z.parquet".into();
    assert_eq!(
        result(&[incomplete, repaired], "rain_amt", start() + 6 * HOUR),
        Some(0.25)
    );
}

#[tokio::test]
async fn strict_source_query_keeps_latest_missing_issue_as_a_blocking_candidate() {
    let directory = tempfile::tempdir().unwrap();
    let old = rain_row(start(), start() + 6 * HOUR, 0.25);
    write_parquet(&directory, std::slice::from_ref(&old), true);
    let mut incomplete = old;
    incomplete.generated = None;
    incomplete.published = Some(start() - HOUR / 2);
    incomplete.source = "2026-01-16/forecasts_2026-01-16T23:30:00Z.parquet".into();
    write_parquet(&directory, &[incomplete], true);
    let access = WeatherAccess::new(Arc::new(crate::file_access::FileAccess::new(
        directory.path().to_string_lossy().into_owned(),
    )));
    let request = ForecastRequest {
        start: Some(datetime!(2026-01-17 00:00 UTC)),
        end: Some(datetime!(2026-01-17 06:00 UTC)),
        generated_start: None,
        generated_end: None,
        station_ids: "KPWM".into(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let result = access
        .forecast_assessment(&request, vec!["KPWM".into()])
        .await
        .unwrap();
    assert!(
        result
            .iter()
            .all(|value| value.value.is_none() && value.reason.is_some())
    );
}

#[test]
fn compatibility_never_suggests_a_malformed_or_shifted_native_boundary() {
    let mut source = rain_row(start(), start() + 6 * HOUR, 0.25);
    source.source_layouts = Some(
        source
            .source_layouts
            .unwrap()
            .replace("2026-01-17T06:00:00Z", "bad end"),
    );
    let assessment = evaluate_selected(
        &[&source],
        &documents(std::slice::from_ref(&source)),
        &["KPWM".into()],
        start(),
        start() + 6 * HOUR,
    );
    assert!(
        assessment
            .iter()
            .all(|metric| metric.native_intervals.is_empty())
    );
}

/// A 24-hour window starting at any hour holds exactly one daytime high and
/// one overnight low, whichever time zone the station's periods follow.
#[test]
fn any_day_long_window_holds_one_high_and_one_low() {
    // Daytime highs 13:00-01:00 UTC and overnight lows 01:00-14:00 UTC, as for
    // a US Central station, for three days.
    let mut rows = vec![];
    for day in 0..3 {
        let noon = start() + day * 24 * HOUR;
        rows.push(row(
            noon + 13 * HOUR,
            noon + 25 * HOUR,
            &[("max_temp", Some(70.0 + day as f64))],
        ));
        rows.push(row(
            noon + HOUR,
            noon + 14 * HOUR,
            &[("min_temp", Some(50.0 - day as f64))],
        ));
    }
    for offset in 0..24 {
        let window_start = start() + 12 * HOUR + offset * HOUR;
        let window_end = window_start + 24 * HOUR;
        let assessed = evaluate(
            &rows,
            &documents(&rows),
            &["KPWM".into()],
            window_start,
            window_end,
        );
        let value = |metric: &str| {
            assessed
                .iter()
                .find(|value| value.metric == metric)
                .unwrap()
                .value
        };
        assert!(
            value("temp_high").is_some(),
            "high for a window {offset} h in"
        );
        assert!(
            value("temp_low").is_some(),
            "low for a window {offset} h in"
        );
    }
}
