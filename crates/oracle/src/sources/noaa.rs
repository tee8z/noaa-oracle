//! NOAA weather: forecasts are the baseline, METAR observations the outcome.
//! Targets are NOAA station ids (e.g. `KORD`); readings come from the
//! parquet files the daemon publishes, in Fahrenheit, inches, knots, and
//! percent.

use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Arc};
use time::{Date, Duration, OffsetDateTime, UtcOffset, macros::format_description};

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

/// NOAA publishes up to a week of forecast periods per issue. Settlement
/// looks this far back for the latest publication before an event.
const BASELINE_LOOKBACK: Duration = Duration::days(7);
/// Provisional readings (and the line history) look back only this far. The
/// daemon publishes hourly; a longer gap leaves the baseline missing until a
/// publication arrives, where settlement would still find an older one.
const PROVISIONAL_LOOKBACK: Duration = Duration::hours(3);

/// Fixed Par rules: temperatures compare whole degrees; wind speed is exact
/// knots; direction is par within 22° either way; rain within 0.1", snow
/// within 0.5", humidity within 5 points (against the forecast maximum).
///
/// Temperatures, wind speed, and humidity can also be scored against fitted
/// lines. Direction wraps around the compass and precipitation is mostly
/// zero, so neither has lines.
const METRICS: &[Metric] = &[
    Metric {
        id: TEMP_HIGH,
        par: ParRule::Rounded,
        calibrated: true,
    },
    Metric {
        id: TEMP_LOW,
        par: ParRule::Rounded,
        calibrated: true,
    },
    Metric {
        id: WIND_SPEED,
        par: ParRule::Exact,
        calibrated: true,
    },
    Metric {
        id: WIND_DIRECTION,
        par: ParRule::Compass(22.0),
        calibrated: false,
    },
    Metric {
        id: RAIN_AMT,
        par: ParRule::Within(0.1),
        calibrated: false,
    },
    Metric {
        id: SNOW_AMT,
        par: ParRule::Within(0.5),
        calibrated: false,
    },
    Metric {
        id: HUMIDITY,
        par: ParRule::Within(5.0),
        calibrated: true,
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

    /// Full days, or a day or night half. NOAA forecasts one daytime high
    /// (7am-7pm local) and one overnight low (7pm-8am) a day, scored in the
    /// window holding their midpoint. A window of at least 24 hours holds one
    /// of each. For every US state, in summer and winter time, highs are
    /// centred from 17:00 to 23:00 UTC and lows from 05:30 to 11:30 UTC, so
    /// 12:00-24:00 UTC holds every station's high and 00:00-12:00 UTC every
    /// station's low. Relative humidity periods don't follow those halves.
    fn check_window(&self, window: ObservationWindow, metrics: &[String]) -> Result<(), String> {
        let length = window.end - window.start;
        if length >= Duration::DAY {
            return Ok(());
        }
        let start = window.start.to_offset(time::UtcOffset::UTC);
        let half = (length == Duration::hours(12)
            && start.time().minute() == 0
            && start.time().second() == 0
            && start.time().nanosecond() == 0)
            .then_some(start.hour())
            .and_then(|hour| match hour {
                12 => Some(("day", "12:00-24:00 UTC", TEMP_LOW)),
                0 => Some(("night", "00:00-12:00 UTC", TEMP_HIGH)),
                _ => None,
            });
        let Some((name, hours, missing)) = half else {
            return Err(
                "the observation window must be at least 24 hours, or a day (12:00-24:00 UTC) or night (00:00-12:00 UTC) half"
                    .into(),
            );
        };
        match metrics
            .iter()
            .find(|metric| *metric == missing || *metric == HUMIDITY)
        {
            Some(metric) => Err(format!(
                "a {name} window ({hours}) cannot score {metric}; it holds no whole {metric} period"
            )),
            None => Ok(()),
        }
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
        self.load_readings(window, targets, None).await
    }

    /// Forecasts published before the daemon recorded native intervals have
    /// no whole-period baseline, and 2.5.0 reset the line history, so lines
    /// had nothing left to fit. Where a window has no native baseline, line
    /// history falls back to the daily roll-up it read before 2.5.0.
    async fn line_readings(
        &self,
        window: ObservationWindow,
        targets: &[String],
    ) -> Result<Vec<Reading>, SourceError> {
        let mut readings = self.load_readings(window, targets, None).await?;
        if window.start >= window.end || readings.iter().all(|r| r.baseline.is_some()) {
            return Ok(readings);
        }
        let request = ForecastRequest {
            start: Some(window.start),
            end: Some(window.end),
            generated_start: Some(window.start.saturating_sub(BASELINE_LOOKBACK)),
            generated_end: Some(window.start.saturating_sub(Duration::nanoseconds(1))),
            station_ids: targets.join(","),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let forecasts = self
            .weather
            .forecasts_data(&request, targets.to_vec())
            .await
            .map_err(unavailable)?;
        for reading in &mut readings {
            if reading.baseline.is_none() {
                reading.baseline =
                    rolled_up_baseline(&reading.target, &reading.metric, window, &forecasts);
            }
        }
        Ok(readings)
    }

    async fn settlement_readings(
        &self,
        window: ObservationWindow,
        targets: &[String],
        required_collected_after: OffsetDateTime,
        metrics: &[String],
    ) -> Result<Vec<Reading>, SourceError> {
        self.load_readings(window, targets, Some((required_collected_after, metrics)))
            .await
    }

    async fn line_targets(&self) -> Result<Vec<String>, SourceError> {
        let mut stations: Vec<String> = self
            .weather
            .stations()
            .await
            .map_err(unavailable)?
            .into_iter()
            .map(|station| station.station_id)
            .filter(|station| validate_station_id(station).is_ok())
            .collect();
        stations.sort();
        stations.dedup();
        Ok(stations)
    }
}

impl NoaaWeather {
    async fn load_readings(
        &self,
        window: ObservationWindow,
        targets: &[String],
        settlement: Option<(OffsetDateTime, &[String])>,
    ) -> Result<Vec<Reading>, SourceError> {
        if window.start >= window.end {
            return Ok(vec![]);
        }
        let forecast_request = ForecastRequest {
            start: Some(window.start),
            end: Some(window.end),
            generated_start: Some(window.start.saturating_sub(BASELINE_LOOKBACK)),
            generated_end: Some(window.start.saturating_sub(Duration::nanoseconds(1))),
            station_ids: targets.join(","),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        let observation_request = ObservationRequest {
            start: Some(window.start),
            end: Some(window.end - Duration::nanoseconds(1)),
            station_ids: targets.join(","),
            temperature_unit: TemperatureUnit::Fahrenheit,
        };
        if let Some((cutoff, metrics)) = settlement {
            // The strict reader uses a half-open event window for point
            // values and also reads the ending accumulation report for rain.
            let observation_request = ObservationRequest {
                end: Some(window.end),
                ..observation_request
            };
            let forecasts = self
                .weather
                .settlement_forecasts(&forecast_request, targets.to_vec())
                .await
                .map_err(unavailable)?;
            let observations = self
                .weather
                .settlement_observations(&observation_request, targets.to_vec(), cutoff, metrics)
                .await
                .map_err(unavailable)?;
            // Strict forecasts already represent the full requested interval.
            // Preserve duplicate metric rows so the lifecycle rejects ambiguity.
            return Ok(forecasts
                .into_iter()
                .map(|forecast| {
                    let mut station = observations
                        .iter()
                        .filter(|row| row.station_id == forecast.station_id);
                    let observation = station.next().filter(|_| station.next().is_none());
                    Reading {
                        target: forecast.station_id,
                        metric: forecast.metric.clone(),
                        baseline: forecast.value,
                        observed: observation
                            .and_then(|row| observed_metric(row, &forecast.metric)),
                    }
                })
                .collect());
        }
        // The same whole-period baseline settlement uses, from the latest
        // publication before the window (or before now, until it opens).
        // Reading only recent publications keeps each refresh cheap.
        let issued_before = window.start.min(OffsetDateTime::now_utc());
        let forecast_request = ForecastRequest {
            generated_start: Some(issued_before.saturating_sub(PROVISIONAL_LOOKBACK)),
            ..forecast_request
        };
        let baselines = self
            .weather
            .forecast_assessment(&forecast_request, targets.to_vec())
            .await
            .map_err(unavailable)?;
        let observations = self
            .weather
            .observation_data(&observation_request, targets.to_vec())
            .await
            .map_err(unavailable)?;
        Ok(baselines
            .into_iter()
            .map(|baseline| {
                let observation = observations
                    .iter()
                    .filter(|row| row.station_id == baseline.station_id)
                    .min_by(|a, b| a.start_time.cmp(&b.start_time));
                Reading {
                    observed: observation.and_then(|row| observed_metric(row, &baseline.metric)),
                    target: baseline.station_id,
                    metric: baseline.metric,
                    baseline: baseline.value,
                }
            })
            .collect())
    }
}

/// The baseline line history used before 2.5.0: over the window's UTC days,
/// the forecast highs' maximum, lows' minimum or wind speeds' maximum, and
/// only when each day has exactly one forecast. Other metrics have none.
fn rolled_up_baseline(
    target: &str,
    metric: &str,
    window: ObservationWindow,
    forecasts: &[Forecast],
) -> Option<f64> {
    let first_day = window.start.to_offset(UtcOffset::UTC).date();
    let last_day = (window.end - Duration::nanoseconds(1))
        .to_offset(UtcOffset::UTC)
        .date();
    let mut daily = BTreeMap::new();
    for forecast in forecasts.iter().filter(|row| row.station_id == target) {
        let Some(date) = forecast
            .date
            .get(..10)
            .and_then(|date| Date::parse(date, format_description!("[year]-[month]-[day]")).ok())
        else {
            continue;
        };
        // A duplicate day is ambiguous.
        if date >= first_day && date <= last_day && daily.insert(date, forecast).is_some() {
            return None;
        }
    }
    if daily.len() as i64 != (last_day - first_day).whole_days() + 1 {
        return None;
    }
    let days = daily.values();
    let value = match metric {
        TEMP_HIGH => days.map(|day| day.temp_high as f64).reduce(f64::max),
        TEMP_LOW => days.map(|day| day.temp_low as f64).reduce(f64::min),
        WIND_SPEED => days
            .map(|day| day.wind_speed.map(|speed| speed as f64))
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .reduce(f64::max),
        _ => None,
    };
    value.filter(|value| value.is_finite())
}

fn observed_metric(observation: &Observation, metric: &str) -> Option<f64> {
    match metric {
        TEMP_HIGH => Some(observation.temp_high),
        TEMP_LOW => Some(observation.temp_low),
        WIND_SPEED => observation.wind_speed.map(|value| value as f64),
        WIND_DIRECTION => observation.wind_direction.map(|value| value as f64),
        RAIN_AMT => observation.rain_amt,
        SNOW_AMT => observation.snow_amt,
        HUMIDITY => observation.humidity.map(|value| value as f64),
        _ => None,
    }
}

fn unavailable(error: weather_data::Error) -> SourceError {
    match error {
        weather_data::Error::DataQuality {
            rejected_reports,
            unverified_reports,
        } => SourceError::DataQuality {
            rejected_reports,
            unverified_reports,
        },
        error => SourceError::Unavailable(Box::new(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::weather_data::{DailyObservation, Error, Forecast, ForecastAssessment, Station};
    use std::sync::Mutex;
    use time::macros::datetime;

    /// Whole-period baselines for every station: a high of 70 and no low.
    #[derive(Default)]
    struct Weather {
        requests: Mutex<Vec<ForecastRequest>>,
    }

    #[async_trait]
    impl WeatherData for Weather {
        async fn forecasts_data(
            &self,
            _: &ForecastRequest,
            _: Vec<String>,
        ) -> Result<Vec<Forecast>, Error> {
            unreachable!("readings use the whole-period baseline")
        }
        async fn forecast_assessment(
            &self,
            request: &ForecastRequest,
            stations: Vec<String>,
        ) -> Result<Vec<ForecastAssessment>, Error> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(stations
                .iter()
                .flat_map(|station| {
                    [(TEMP_HIGH, Some(70.0)), (TEMP_LOW, None)].map(|(metric, value)| {
                        ForecastAssessment {
                            station_id: station.clone(),
                            metric: metric.into(),
                            value,
                            reason: value.is_none().then(|| "no forecast".into()),
                            native_intervals: vec![],
                        }
                    })
                })
                .collect())
        }
        async fn observation_data(
            &self,
            _: &ObservationRequest,
            _: Vec<String>,
        ) -> Result<Vec<Observation>, Error> {
            Ok(vec![Observation {
                station_id: "KORD".into(),
                start_time: "2026-01-01T00:00:00Z".into(),
                end_time: "2026-01-01T23:00:00Z".into(),
                temp_low: 50.0,
                temp_high: 72.4,
                latest_temp: None,
                latest_temp_time: None,
                wind_speed: Some(9),
                temp_unit_code: "fahrenheit".into(),
                wind_direction: None,
                humidity: None,
                rain_amt: None,
                snow_amt: None,
                ice_amt: None,
            }])
        }
        async fn daily_observations(
            &self,
            _: &ObservationRequest,
            _: Vec<String>,
        ) -> Result<Vec<DailyObservation>, Error> {
            Ok(vec![])
        }
        async fn stations(&self) -> Result<Vec<Station>, Error> {
            Ok(vec![])
        }
    }

    #[test]
    fn full_days_and_day_or_night_halves_can_be_attested() {
        let noaa = NoaaWeather::new(Arc::new(Weather::default()));
        let window = |start: OffsetDateTime, hours| ObservationWindow {
            start,
            end: start + Duration::hours(hours),
        };
        let metrics = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
        let full = metrics(&[TEMP_HIGH, TEMP_LOW, WIND_SPEED]);
        let noon = datetime!(2026-01-01 12:00 UTC);
        let midnight = datetime!(2026-01-01 00:00 UTC);
        assert!(
            noaa.check_window(window(datetime!(2026-01-01 05:17 UTC), 24), &full)
                .is_ok()
        );
        assert!(noaa.check_window(window(midnight, 72), &full).is_ok());
        assert!(
            noaa.check_window(window(noon, 12), &metrics(&[TEMP_HIGH, WIND_SPEED]))
                .is_ok()
        );
        assert!(
            noaa.check_window(
                window(midnight, 12),
                &metrics(&[TEMP_LOW, WIND_SPEED, RAIN_AMT])
            )
            .is_ok()
        );
        let day = noaa.check_window(window(noon, 12), &full).unwrap_err();
        assert!(
            day.contains("day window") && day.contains(TEMP_LOW),
            "{day}"
        );
        let night = noaa.check_window(window(midnight, 12), &full).unwrap_err();
        assert!(
            night.contains("night window") && night.contains(TEMP_HIGH),
            "{night}"
        );
        assert!(
            noaa.check_window(window(noon, 12), &metrics(&[HUMIDITY]))
                .is_err()
        );
        for (start, hours) in [
            (datetime!(2026-01-01 13:00 UTC), 12),
            (noon, 13),
            (noon, 2),
            (datetime!(2026-01-01 12:00:30 UTC), 12),
        ] {
            assert!(
                noaa.check_window(window(start, hours), &metrics(&[WIND_SPEED]))
                    .is_err(),
                "{start} for {hours} h"
            );
        }
    }

    /// Provisional readings (and so the line history) score against the
    /// baseline settlement uses, read from the last few hours of publications.
    #[tokio::test]
    async fn readings_use_the_whole_period_baseline_from_recent_publications() {
        let weather = Arc::new(Weather::default());
        let noaa = NoaaWeather::new(weather.clone());
        let start = datetime!(2026-01-01 00:00 UTC);
        let window = ObservationWindow {
            start,
            end: start + Duration::DAY,
        };
        let readings = noaa
            .readings(window, &["KORD".into(), "KDEN".into()])
            .await
            .unwrap();
        let find = |target: &str, metric: &str| {
            readings
                .iter()
                .find(|reading| reading.target == target && reading.metric == metric)
                .unwrap()
                .clone()
        };
        let high = find("KORD", TEMP_HIGH);
        assert_eq!((high.baseline, high.observed), (Some(70.0), Some(72.4)));
        assert_eq!(find("KORD", TEMP_LOW).baseline, None);
        assert_eq!(find("KDEN", TEMP_HIGH).observed, None);
        let request = weather.requests.lock().unwrap()[0].clone();
        assert_eq!(request.generated_start, Some(start - PROVISIONAL_LOOKBACK));
        assert_eq!(
            request.generated_end,
            Some(start - Duration::nanoseconds(1))
        );
    }
}
