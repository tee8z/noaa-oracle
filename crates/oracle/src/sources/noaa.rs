//! NOAA weather: forecasts are the baseline, METAR observations the outcome.
//! Targets are NOAA station ids (e.g. `KORD`); readings come from the
//! parquet files the daemon publishes, in Fahrenheit, inches, knots, and
//! percent.

use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Arc};
use time::{Date, Duration, UtcOffset, macros::format_description};

use super::{Metric, ObservationWindow, OutcomeSource, ParRule, Reading, SourceError, SourceId};
use crate::{
    routes::{ForecastRequest, ObservationRequest, TemperatureUnit},
    weather_data::{self, Forecast, Observation, WeatherData, validate_station_id},
};

pub const NOAA_WEATHER: SourceId = SourceId::new("noaa_weather");

pub const TEMP_HIGH: &str = "temp_high";
pub const TEMP_LOW: &str = "temp_low";
pub const WIND_SPEED: &str = "wind_speed";
pub const WIND_DIRECTION: &str = "wind_direction";
pub const RAIN_AMT: &str = "rain_amt";
pub const SNOW_AMT: &str = "snow_amt";
pub const HUMIDITY: &str = "humidity";

/// NOAA publishes up to a week of forecast periods per issue.
const BASELINE_LOOKBACK: Duration = Duration::days(7);

/// Par rules: temperatures compare whole degrees; wind speed is exact knots;
/// direction is par within 22° either way; rain within 0.1", snow within
/// 0.5", humidity within 5 points (against the forecast maximum).
const METRICS: &[Metric] = &[
    Metric {
        id: TEMP_HIGH,
        par: ParRule::Rounded,
    },
    Metric {
        id: TEMP_LOW,
        par: ParRule::Rounded,
    },
    Metric {
        id: WIND_SPEED,
        par: ParRule::Exact,
    },
    Metric {
        id: WIND_DIRECTION,
        par: ParRule::Compass(22.0),
    },
    Metric {
        id: RAIN_AMT,
        par: ParRule::Within(0.1),
    },
    Metric {
        id: SNOW_AMT,
        par: ParRule::Within(0.5),
    },
    Metric {
        id: HUMIDITY,
        par: ParRule::Within(5.0),
    },
];

pub struct NoaaWeather {
    weather: Arc<dyn WeatherData>,
}

impl NoaaWeather {
    pub fn new(weather: Arc<dyn WeatherData>) -> Self {
        Self { weather }
    }
}

#[async_trait]
impl OutcomeSource for NoaaWeather {
    fn id(&self) -> SourceId {
        NOAA_WEATHER
    }

    fn metrics(&self) -> &'static [Metric] {
        METRICS
    }

    fn default_metrics(&self) -> Vec<&'static str> {
        vec![TEMP_HIGH, TEMP_LOW, WIND_SPEED]
    }

    fn validate_target(&self, target: &str) -> Result<(), SourceError> {
        validate_station_id(target).map_err(|error| SourceError::InvalidTarget {
            target: target.to_owned(),
            reason: error.to_string(),
        })
    }

    async fn readings(
        &self,
        window: ObservationWindow,
        targets: &[String],
    ) -> Result<Vec<Reading>, SourceError> {
        if window.start >= window.end {
            return Ok(vec![]);
        }
        let station_ids = targets.join(",");
        let forecasts = self
            .weather
            .forecasts_data(
                &ForecastRequest {
                    start: Some(window.start),
                    end: Some(window.end),
                    generated_start: Some(window.start.saturating_sub(BASELINE_LOOKBACK)),
                    generated_end: Some(window.start - Duration::nanoseconds(1)),
                    station_ids: station_ids.clone(),
                    temperature_unit: TemperatureUnit::Fahrenheit,
                },
                targets.to_vec(),
            )
            .await
            .map_err(unavailable)?;
        let observations = self
            .weather
            .observation_data(
                &ObservationRequest {
                    start: Some(window.start),
                    end: Some(window.end - Duration::nanoseconds(1)),
                    station_ids,
                    temperature_unit: TemperatureUnit::Fahrenheit,
                },
                targets.to_vec(),
            )
            .await
            .map_err(unavailable)?;
        Ok(targets
            .iter()
            .flat_map(|target| station_readings(target, window, &forecasts, &observations))
            .collect())
    }
}

fn unavailable(error: weather_data::Error) -> SourceError {
    SourceError::Unavailable(Box::new(error))
}

/// Readings for one station over the event's UTC days. Every day must have
/// a forecast before an aggregate can be compared with the full observed
/// window. Missing metrics on any day keep that metric's baseline missing.
///
/// Missing values stay missing and earn no points for anyone:
/// - NDFD/DWML forecasts mark missing values with `xsi:nil="true"`; nil is
///   "no forecast", not zero
///   (<https://graphical.weather.gov/xml/mdl/XML/Design/MDL_XML_Design.htm>).
/// - METAR fields are all optional, and `wind_dir_degrees` may be `VRB`
///   (variable); calm is reported explicitly as 0° at 0 kt, so an absent
///   direction is not north (<https://aviationweather.gov/data/schema/metar2_0.xsd>).
/// - METAR has no humidity; the parquet query derives it from temperature
///   and dewpoint, so a missing dewpoint means unknown humidity.
/// - Hourly precipitation is omitted when none fell at AO2 stations only;
///   the daemon records that as 0 and leaves other stations missing.
fn station_readings(
    target: &str,
    window: ObservationWindow,
    forecasts: &[Forecast],
    observations: &[Observation],
) -> Vec<Reading> {
    if window.start >= window.end {
        return vec![];
    }
    let first_day = window.start.to_offset(UtcOffset::UTC).date();
    let last_day = (window.end - Duration::nanoseconds(1))
        .to_offset(UtcOffset::UTC)
        .date();
    let expected_days = (last_day - first_day).whole_days() + 1;
    let mut daily = BTreeMap::new();
    let mut unique = true;
    for forecast in forecasts.iter().filter(|row| row.station_id == target) {
        let Some(date) = forecast
            .date
            .get(..10)
            .and_then(|date| Date::parse(date, format_description!("[year]-[month]-[day]")).ok())
        else {
            continue;
        };
        if date >= first_day && date <= last_day {
            // A duplicate daily result is ambiguous. Do not make scoring
            // depend on which row happened to arrive last.
            unique &= daily.insert(date, forecast).is_none();
        }
    }
    if daily.is_empty() {
        return vec![];
    }
    let complete = unique && daily.len() as i64 == expected_days;
    let daily: Vec<&Forecast> = daily.into_values().collect();
    let observation = observations
        .iter()
        .filter(|observation| observation.station_id == target)
        .min_by(|a, b| a.start_time.cmp(&b.start_time));
    let observed = |value: fn(&Observation) -> Option<f64>| observation.and_then(value);
    let reading = |metric: &str, baseline: Option<f64>, observed: Option<f64>| Reading {
        target: target.to_owned(),
        metric: metric.to_owned(),
        baseline,
        observed,
    };
    let aggregate = |field: fn(&Forecast) -> Option<f64>, reduce: fn(f64, f64) -> f64| {
        if !complete {
            return None;
        }
        daily
            .iter()
            .map(|forecast| field(forecast).filter(|value| value.is_finite()))
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .reduce(reduce)
            .filter(|value| value.is_finite())
    };
    let wind_speed = aggregate(
        |forecast| forecast.wind_speed.map(|value| value as f64),
        f64::max,
    );
    // Daily query directions belong to that day's maximum wind. Use the
    // same pair across the event, breaking equal-speed ties by latest UTC
    // day. A missing direction at the peak stays missing.
    let wind_direction = wind_speed
        .and_then(|speed| {
            daily
                .iter()
                .rev()
                .find(|forecast| forecast.wind_speed.map(|value| value as f64) == Some(speed))
        })
        .and_then(|forecast| forecast.wind_direction)
        .map(|direction| direction as f64);
    vec![
        reading(
            TEMP_HIGH,
            aggregate(|forecast| Some(forecast.temp_high as f64), f64::max),
            observed(|o| Some(o.temp_high)),
        ),
        reading(
            TEMP_LOW,
            aggregate(|forecast| Some(forecast.temp_low as f64), f64::min),
            observed(|o| Some(o.temp_low)),
        ),
        reading(
            WIND_SPEED,
            wind_speed,
            observed(|o| o.wind_speed.map(|value| value as f64)),
        ),
        reading(
            WIND_DIRECTION,
            wind_direction,
            observed(|o| o.wind_direction.map(|value| value as f64)),
        ),
        reading(
            RAIN_AMT,
            aggregate(|forecast| forecast.rain_amt, |a, b| a + b),
            observed(|o| o.rain_amt),
        ),
        reading(
            SNOW_AMT,
            aggregate(|forecast| forecast.snow_amt, |a, b| a + b),
            observed(|o| o.snow_amt),
        ),
        reading(
            HUMIDITY,
            aggregate(
                |forecast| forecast.humidity_max.map(|value| value as f64),
                f64::max,
            ),
            observed(|o| o.humidity.map(|value| value as f64)),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};

    fn window(start: &str, end: &str) -> ObservationWindow {
        ObservationWindow {
            start: OffsetDateTime::parse(start, &Rfc3339).unwrap(),
            end: OffsetDateTime::parse(end, &Rfc3339).unwrap(),
        }
    }

    fn baseline(readings: &[Reading], metric: &str) -> Option<f64> {
        readings
            .iter()
            .find(|reading| reading.metric == metric)
            .unwrap()
            .baseline
    }

    fn forecast(station: &str, date: &str, temp_high: i64) -> Forecast {
        Forecast {
            station_id: station.into(),
            date: date.into(),
            start_time: String::new(),
            end_time: String::new(),
            temp_low: 40,
            temp_high,
            wind_speed: None,
            wind_direction: None,
            humidity_max: Some(80),
            humidity_min: None,
            temp_unit_code: "fahrenheit".into(),
            precip_chance: None,
            rain_amt: None,
            snow_amt: None,
            ice_amt: None,
        }
    }

    #[test]
    fn only_forecast_days_inside_the_utc_window_are_compared() {
        let window = window("2030-01-01T02:00:00+02:00", "2030-01-02T02:00:00+02:00");
        let forecasts = vec![
            forecast("KORD", "2030-01-02", 55),
            forecast("KORD", "2030-01-01 00:00:00", 50),
            forecast("KORD", "2029-12-31", 90),
            forecast("KSAW", "2030-01-01", 10),
        ];
        let readings = station_readings("KORD", window, &forecasts, &[]);
        let temp_high = readings.iter().find(|r| r.metric == TEMP_HIGH).unwrap();
        assert_eq!(temp_high.baseline, Some(50.0));
        assert_eq!(temp_high.observed, None);
        let wind = readings.iter().find(|r| r.metric == WIND_SPEED).unwrap();
        assert_eq!(wind.baseline, None, "a nil forecast is not calm");
        assert!(station_readings("KMSP", window, &forecasts, &[]).is_empty());
    }

    #[test]
    fn multi_day_baselines_aggregate_the_same_days_as_observations() {
        let window = window("2030-01-01T00:00:00Z", "2030-01-03T00:00:00Z");
        let mut first = forecast("KORD", "2030-01-01", 50);
        first.wind_speed = Some(10);
        first.rain_amt = Some(0.25);
        first.snow_amt = Some(0.0);
        let mut second = forecast("KORD", "2030-01-02", 70);
        second.temp_low = 30;
        second.wind_speed = Some(15);
        second.humidity_max = Some(90);
        second.rain_amt = Some(0.5);
        second.snow_amt = Some(1.0);
        let forecasts = [second, first];
        let readings = station_readings("KORD", window, &forecasts, &[]);
        assert_eq!(baseline(&readings, TEMP_HIGH), Some(70.0));
        assert_eq!(baseline(&readings, TEMP_LOW), Some(30.0));
        assert_eq!(baseline(&readings, WIND_SPEED), Some(15.0));
        assert_eq!(baseline(&readings, HUMIDITY), Some(90.0));
        assert_eq!(baseline(&readings, RAIN_AMT), Some(0.75));
        assert_eq!(baseline(&readings, SNOW_AMT), Some(1.0));
    }

    #[test]
    fn missing_daily_coverage_never_becomes_a_partial_baseline() {
        let window = window("2030-01-01T12:00:00Z", "2030-01-03T12:00:00Z");
        let forecasts = [
            forecast("KORD", "2030-01-01", 50),
            forecast("KORD", "2030-01-03", 70),
        ];
        let readings = station_readings("KORD", window, &forecasts, &[]);
        assert!(!readings.is_empty());
        assert!(readings.iter().all(|reading| reading.baseline.is_none()));
    }

    #[test]
    fn missing_daily_precipitation_does_not_erase_other_complete_metrics() {
        let window = window("2030-01-01T00:00:00Z", "2030-01-03T00:00:00Z");
        let mut first = forecast("KORD", "2030-01-01", 50);
        first.rain_amt = Some(0.25);
        first.snow_amt = Some(0.0);
        let mut second = forecast("KORD", "2030-01-02", 70);
        second.snow_amt = Some(0.0);
        let readings = station_readings("KORD", window, &[first, second], &[]);
        assert_eq!(baseline(&readings, RAIN_AMT), None);
        assert_eq!(baseline(&readings, SNOW_AMT), Some(0.0));
        assert_eq!(baseline(&readings, TEMP_HIGH), Some(70.0));
    }

    #[test]
    fn wind_direction_belongs_to_the_strongest_wind_not_the_largest_bearing() {
        let window = window("2030-01-01T00:00:00Z", "2030-01-03T00:00:00Z");
        let mut first = forecast("KORD", "2030-01-01", 50);
        first.wind_speed = Some(5);
        first.wind_direction = Some(350);
        let mut second = forecast("KORD", "2030-01-02", 70);
        second.wind_speed = Some(20);
        second.wind_direction = Some(10);
        let mut forecasts = [second, first];
        let readings = station_readings("KORD", window, &forecasts, &[]);
        assert_eq!(baseline(&readings, WIND_SPEED), Some(20.0));
        assert_eq!(baseline(&readings, WIND_DIRECTION), Some(10.0));

        forecasts[0].wind_direction = None;
        let missing = station_readings("KORD", window, &forecasts, &[]);
        assert_eq!(baseline(&missing, WIND_DIRECTION), None);

        forecasts[0].wind_direction = Some(10);
        forecasts[1].wind_speed = None;
        let incomplete = station_readings("KORD", window, &forecasts, &[]);
        assert_eq!(baseline(&incomplete, WIND_SPEED), None);
        assert_eq!(baseline(&incomplete, WIND_DIRECTION), None);
    }

    #[test]
    fn equal_peak_winds_use_the_latest_day_even_when_its_direction_is_missing() {
        let window = window("2030-01-01T00:00:00Z", "2030-01-03T00:00:00Z");
        let mut first = forecast("KORD", "2030-01-01", 50);
        first.wind_speed = Some(20);
        first.wind_direction = Some(350);
        let mut second = forecast("KORD", "2030-01-02", 70);
        second.wind_speed = Some(20);
        second.wind_direction = Some(10);
        let mut forecasts = [second, first];
        let readings = station_readings("KORD", window, &forecasts, &[]);
        assert_eq!(baseline(&readings, WIND_DIRECTION), Some(10.0));
        forecasts.reverse();
        let reordered = station_readings("KORD", window, &forecasts, &[]);
        assert_eq!(readings, reordered);
        forecasts[1].wind_direction = None;
        let missing = station_readings("KORD", window, &forecasts, &[]);
        assert_eq!(baseline(&missing, WIND_DIRECTION), None);
    }

    #[test]
    fn duplicate_daily_forecasts_cannot_make_scoring_depend_on_query_order() {
        let window = window("2030-01-01T00:00:00Z", "2030-01-02T00:00:00Z");
        let mut forecasts = [
            forecast("KORD", "2030-01-01", 50),
            forecast("KORD", "2030-01-01", 70),
        ];
        let first = station_readings("KORD", window, &forecasts, &[]);
        forecasts.reverse();
        let second = station_readings("KORD", window, &forecasts, &[]);
        assert_eq!(first, second);
        assert!(first.iter().all(|reading| reading.baseline.is_none()));
    }

    #[test]
    fn every_metric_has_a_reading() {
        let readings = station_readings(
            "KORD",
            window("2030-01-01T00:00:00Z", "2030-01-02T00:00:00Z"),
            &[forecast("KORD", "2030-01-01", 50)],
            &[],
        );
        for metric in METRICS {
            assert!(readings.iter().any(|reading| reading.metric == metric.id));
        }
    }
}
