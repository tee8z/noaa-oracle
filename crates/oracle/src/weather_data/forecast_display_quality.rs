//! Quality accompanies the exact rows used for a calendar display. Native
//! extrema over different periods must not become an inverted daily range.

use super::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ForecastQuality {
    pub rejected_rows: u64,
    pub unverified_rows: u64,
    pub unavailable: bool,
    pub range_issues: Vec<ForecastRangeIssue>,
    #[serde(skip)]
    daily_counts: Vec<(String, u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ForecastRangeIssue {
    pub station_id: String,
    pub date: String,
    pub metric: String,
    pub minimum: f64,
    pub maximum: f64,
    pub native_intervals: Vec<ForecastNativeInterval>,
}

impl ForecastQuality {
    pub(super) fn extend(&mut self, other: Self) {
        self.rejected_rows += other.rejected_rows;
        self.unverified_rows += other.unverified_rows;
        self.unavailable |= other.unavailable;
        self.range_issues.extend(other.range_issues);
        self.daily_counts.extend(other.daily_counts);
    }

    pub fn unavailable() -> Self {
        Self {
            unavailable: true,
            ..Self::default()
        }
    }

    /// A source period can cross the requested midnight. Match the calendar
    /// rows the caller actually displays before combining past/future panels.
    pub fn retain_days(&mut self, start: &str, end: &str) {
        let inside = |date: &str| {
            let date = date.get(..10).unwrap_or(date);
            date >= start && date < end
        };
        self.daily_counts.retain(|(date, _, _)| inside(date));
        self.rejected_rows = self.daily_counts.iter().map(|(_, count, _)| count).sum();
        self.unverified_rows = self.daily_counts.iter().map(|(_, _, count)| count).sum();
        self.range_issues.retain(|issue| inside(&issue.date));
    }
}

#[derive(Deserialize)]
struct Period {
    start: String,
    end: String,
    version: Option<String>,
    layouts: Option<String>,
}

#[derive(Deserialize)]
struct NativePeriod {
    start: String,
    end: Option<String>,
}

fn native_period(json: Option<String>, metric: &str) -> Option<ForecastNativeInterval> {
    let period: Period = serde_json::from_str(&json?).ok()?;
    if period.version.as_deref() != Some("ndfd-native-v1") {
        return None;
    }
    let layouts: BTreeMap<String, NativePeriod> = serde_json::from_str(&period.layouts?).ok()?;
    let native = layouts.get(metric)?;
    let parse = |value: &str| OffsetDateTime::parse(value, &Rfc3339).ok();
    let start = parse(&native.start)?;
    let end = parse(native.end.as_deref()?)?;
    if end <= start || Some(start) != parse(&period.start) || Some(end) != parse(&period.end) {
        return None;
    }
    Some(ForecastNativeInterval {
        metric: metric.into(),
        start: start.to_offset(UtcOffset::UTC).format(&Rfc3339).ok()?,
        end: Some(end.to_offset(UtcOffset::UTC).format(&Rfc3339).ok()?),
    })
}

pub(super) fn decode(batches: &[RecordBatch]) -> Result<ForecastQuality, Error> {
    let mut quality = ForecastQuality::default();
    for batch in batches {
        let stations = strings(batch, "station_id")?;
        let dates = strings(batch, "date")?;
        let rejected = integers(batch, "rejected_rows")?;
        let unverified = integers(batch, "unverified_rows")?;
        for row in 0..batch.num_rows() {
            let rejected = integer(rejected, row).unwrap_or_default().max(0) as u64;
            let unverified = integer(unverified, row).unwrap_or_default().max(0) as u64;
            quality.rejected_rows += rejected;
            quality.unverified_rows += unverified;
            quality
                .daily_counts
                .push((dates.value(row).into(), rejected, unverified));
            for (metric, minimum, maximum, min_period, max_period, min_name, max_name) in [
                (
                    "temperature",
                    "raw_temp_low",
                    "raw_temp_high",
                    "min_temp_period",
                    "max_temp_period",
                    "min_temp",
                    "max_temp",
                ),
                (
                    "humidity",
                    "raw_humidity_min",
                    "raw_humidity_max",
                    "humidity_min_period",
                    "humidity_max_period",
                    "relative_humidity_min",
                    "relative_humidity_max",
                ),
            ] {
                let minimum = double(doubles(batch, minimum)?, row);
                let maximum = double(doubles(batch, maximum)?, row);
                if let Some((minimum, maximum)) = minimum
                    .zip(maximum)
                    .filter(|(minimum, maximum)| minimum > maximum)
                {
                    quality.range_issues.push(ForecastRangeIssue {
                        station_id: stations.value(row).into(),
                        date: dates.value(row).into(),
                        metric: metric.into(),
                        minimum,
                        maximum,
                        native_intervals: [
                            native_period(text(strings(batch, min_period)?, row), min_name),
                            native_period(text(strings(batch, max_period)?, row), max_name),
                        ]
                        .into_iter()
                        .flatten()
                        .collect(),
                    });
                }
            }
        }
    }
    Ok(quality)
}
