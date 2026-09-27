//! NOAA weather: forecasts are the baseline, METAR observations the outcome.
//! Targets are NOAA station ids (e.g. `KORD`); readings come from the
//! parquet files the daemon publishes, in Fahrenheit, inches, knots, and
//! percent.

use async_trait::async_trait;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};

use super::{Metric, ObservationWindow, OutcomeSource, ParRule, Reading, SourceError, SourceId};
use crate::{
    routes::{ForecastRequest, ObservationRequest, TemperatureUnit},
    weather_data::{self, Observation, WeatherData, validate_station_id},
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

    async fn settlement_readings(
        &self,
        window: ObservationWindow,
        targets: &[String],
        required_collected_after: OffsetDateTime,
    ) -> Result<Vec<Reading>, SourceError> {
        self.load_readings(window, targets, Some(required_collected_after))
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
        required_collected_after: Option<OffsetDateTime>,
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
        if let Some(cutoff) = required_collected_after {
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
                .settlement_observations(&observation_request, targets.to_vec(), cutoff)
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
