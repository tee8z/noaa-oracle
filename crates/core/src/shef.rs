//! Strict replay of the ASOS RR7 hourly precipitation subset of SHEF.
//! NWS documents PPH as a discrete 60-minute accumulation ending at H+00:
//! https://www.weather.gov/asos/InformationReporting.html
use anyhow::{Result, anyhow, ensure};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use time::{Date, Duration, Month, OffsetDateTime, UtcOffset};

#[derive(Clone, Debug, PartialEq)]
pub struct HourlyPrecipitation {
    pub source_station_id: String,
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    pub liquid_in: Option<f64>,
    pub status: String,
    pub reason: Option<String>,
}

/// Accept only the documented ASOS .A/.AR hourly PPH report, in UTC and
/// inches. Unsupported timezones, parameter modifiers, continuations and
/// additional fields fail explicitly; they cannot change interval semantics.
pub fn parse(product_text: &str, issued_at: OffsetDateTime) -> Result<Vec<HourlyPrecipitation>> {
    let issued_at = issued_at.to_offset(UtcOffset::UTC);
    let mut reports = Vec::new();
    for line in product_text
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('.'))
    {
        let (identity, amount) = line
            .split_once('/')
            .ok_or_else(|| anyhow!("SHEF report has no PPH field"))?;
        let fields: Vec<_> = identity.split_whitespace().collect();
        ensure!(
            matches!(fields.first().copied(), Some(".A" | ".AR")),
            "unsupported SHEF message type"
        );
        ensure!(
            matches!(fields.len(), 4 | 5),
            "unsupported SHEF time or unit modifiers"
        );
        let station = fields[1];
        ensure!(
            (3..=5).contains(&station.len())
                && station
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()),
            "invalid SHEF station identifier"
        );
        if fields.len() == 5 {
            ensure!(fields[3] == "Z", "SHEF timezone must be UTC");
        }
        let stamp = *fields.last().unwrap();
        ensure!(
            stamp.len() == 6
                && stamp.starts_with("DH")
                && stamp[2..].bytes().all(|byte| byte.is_ascii_digit()),
            "unsupported SHEF observation time"
        );
        let hour: u8 = stamp[2..4].parse()?;
        ensure!(
            hour <= 24 && &stamp[4..] == "00",
            "RR7 observation must end on a whole UTC hour"
        );
        let date = parse_date(fields[2], issued_at)?;
        let end = date.with_hms(hour % 24, 0, 0)?.assume_utc()
            + if hour == 24 {
                Duration::DAY
            } else {
                Duration::ZERO
            };
        ensure!(
            end <= issued_at && issued_at - end <= Duration::days(2),
            "SHEF observation time conflicts with bulletin issuance"
        );
        let values: Vec<_> = amount.split_whitespace().collect();
        ensure!(
            values.len() == 2 && values[0] == "PPH",
            "only an unmodified PPH amount in inches is supported"
        );
        let (liquid_in, status, reason) = match values[1] {
            "M" => (
                None,
                "missing",
                Some("source reports a missing hourly amount".into()),
            ),
            "T" => (
                None,
                "missing",
                Some("trace precipitation is not an exact measurable total".into()),
            ),
            value => match value.parse::<f64>() {
                Ok(value) if value.is_finite() && (0.0..=5.0).contains(&value) => {
                    (Some(value), "validated", None)
                }
                Ok(value) if value.is_finite() => (
                    Some(value),
                    "rejected",
                    Some("hourly amount outside the 0..5 inch review bounds".into()),
                ),
                _ => (
                    None,
                    "rejected",
                    Some("source hourly amount is malformed or nonfinite".into()),
                ),
            },
        };
        reports.push(HourlyPrecipitation {
            source_station_id: station.into(),
            start: end - Duration::HOUR,
            end,
            liquid_in,
            status: status.into(),
            reason,
        });
    }
    ensure!(
        !reports.is_empty(),
        "RR7 bulletin has no supported SHEF reports"
    );
    Ok(reports)
}

fn parse_date(value: &str, issued_at: OffsetDateTime) -> Result<Date> {
    ensure!(
        matches!(value.len(), 4 | 8) && value.bytes().all(|byte| byte.is_ascii_digit()),
        "unsupported SHEF date"
    );
    let year = if value.len() == 8 {
        value[..4].parse()?
    } else {
        issued_at.year()
    };
    let tail = &value[value.len() - 4..];
    let month = Month::try_from(tail[..2].parse::<u8>()?)?;
    let day: u8 = tail[2..].parse()?;
    let candidates = if value.len() == 8 {
        vec![year]
    } else {
        vec![year - 1, year, year + 1]
    };
    candidates
        .into_iter()
        .filter_map(|year| Date::from_calendar_date(year, month, day).ok())
        .min_by_key(|date| (*date - issued_at.date()).whole_days().abs())
        .ok_or_else(|| anyhow!("invalid SHEF calendar date"))
}

#[derive(Deserialize)]
struct Catalog {
    data_source: CatalogSource,
    errors: String,
    warnings: String,
    data: CatalogData,
}
#[derive(Deserialize)]
struct CatalogSource {
    #[serde(rename = "@name")]
    name: String,
}
#[derive(Deserialize)]
struct CatalogData {
    #[serde(rename = "@num_results")]
    count: usize,
    #[serde(rename = "Station", default)]
    stations: Vec<CatalogStation>,
}
#[derive(Deserialize)]
struct CatalogStation {
    station_id: String,
    iata_id: Option<String>,
    country: Option<String>,
}

/// All exact station IDs and IATA aliases in the retained official catalog.
/// An alias is usable only when its vector contains exactly one station.
pub fn catalog_aliases(xml: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let catalog: Catalog = serde_xml_rs::SerdeXml::new()
        .overlapping_sequences(true)
        .from_str(xml)?;
    ensure!(
        matches!(catalog.data_source.name.as_str(), "station" | "stations")
            && catalog.errors.trim().is_empty()
            && catalog.warnings.trim().is_empty(),
        "station catalog has source errors"
    );
    ensure!(
        catalog.data.count == catalog.data.stations.len(),
        "station catalog report count does not match"
    );
    let mut aliases = BTreeMap::<String, BTreeSet<String>>::new();
    for station in catalog
        .data
        .stations
        .into_iter()
        .filter(|station| station.country.as_deref() == Some("US"))
    {
        for alias in std::iter::once(&station.station_id).chain(station.iata_id.as_ref()) {
            if !alias.is_empty()
                && alias
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            {
                aliases
                    .entry(alias.clone())
                    .or_default()
                    .insert(station.station_id.clone());
            }
        }
    }
    Ok(aliases
        .into_iter()
        .map(|(alias, stations)| (alias, stations.into_iter().collect()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;
    #[test]
    fn positive_zero_missing_and_trace_preserve_discrete_utc_hours() {
        for (value, amount, status) in [
            ("0.14", Some(0.14), "validated"),
            ("0.00", Some(0.0), "validated"),
            ("M", None, "missing"),
            ("T", None, "missing"),
            ("NaN", None, "rejected"),
            ("-1", Some(-1.0), "rejected"),
        ] {
            let rows = parse(
                &format!("SRUS71 KOKX 270600\nRR7NYC\n.A NYC 0927 DH0600/PPH {value}"),
                datetime!(2026-09-27 06:00 UTC),
            )
            .unwrap();
            assert_eq!(rows[0].start, datetime!(2026-09-27 05:00 UTC));
            assert_eq!(rows[0].end, datetime!(2026-09-27 06:00 UTC));
            assert_eq!(rows[0].liquid_in, amount);
            assert_eq!(rows[0].status, status);
        }
    }
    #[test]
    fn date_rollover_and_explicit_utc_are_supported_but_other_semantics_are_not() {
        let rows = parse(
            ".AR NYC 1231 Z DH2400/PPH 0.12",
            datetime!(2027-01-01 00:02 UTC),
        )
        .unwrap();
        assert_eq!(rows[0].end, datetime!(2027-01-01 00:00 UTC));
        for raw in [
            ".A NYC 0101 E DH0000/PPH 0.00",
            ".A NYC 0101 DH0051/PPH 0.00",
            ".A NYC 0101 DH0000/PPD 0.00",
            ".A NYC 0101 DH0000/PPH 0.00/PPD 0.02",
            ".E NYC 0101 DH0000/PPH 0.00",
            ".A NYC 0101 DH2500/PPH 0.00",
            ".A NYC 0101 DH0100/PPH 0.00",
        ] {
            assert!(
                parse(raw, datetime!(2027-01-01 00:02 UTC)).is_err(),
                "{raw}"
            );
        }
    }
    #[test]
    fn catalog_aliases_never_guess_an_icao_prefix_and_keep_ambiguity() {
        let xml = "<response><data_source name=\"stations\"/><errors/><warnings/><data num_results=\"3\"><Station><station_id>PANC</station_id><iata_id>ANC</iata_id><country>US</country></Station><Station><station_id>KABC</station_id><iata_id>ABC</iata_id><country>US</country></Station><Station><station_id>PABC</station_id><iata_id>ABC</iata_id><country>US</country></Station></data></response>";
        let aliases = catalog_aliases(xml).unwrap();
        assert_eq!(aliases["ANC"], ["PANC"]);
        assert_eq!(aliases["ABC"], ["KABC", "PABC"]);
        assert_eq!(aliases["PANC"], ["PANC"]);
        assert!(catalog_aliases(&xml.replace("num_results=\"3\"", "num_results=\"4\"")).is_err());
    }
}
