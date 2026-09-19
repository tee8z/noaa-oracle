//! NOAA weather: forecasts are the baseline, METAR observations the outcome.
//! Targets are NOAA station ids (e.g. `KORD`); readings come from the
//! parquet files the daemon publishes, in Fahrenheit, inches, knots, and
//! percent.

use async_trait::async_trait;
use std::sync::Arc;

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
        let station_ids = targets.join(",");
        let forecasts = self
            .weather
            .forecasts_data(
                &ForecastRequest {
                    start: Some(window.start),
                    end: Some(window.end),
                    generated_start: None,
                    generated_end: None,
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
                    end: Some(window.end),
                    station_ids,
                    temperature_unit: TemperatureUnit::Fahrenheit,
                },
                targets.to_vec(),
            )
            .await
            .map_err(unavailable)?;
        Ok(targets
            .iter()
            .flat_map(|target| station_readings(target, &forecasts, &observations))
            .collect())
    }
}

fn unavailable(error: weather_data::Error) -> SourceError {
    SourceError::Unavailable(Box::new(error))
}

/// Readings for one station. A station needs a forecast to be scored; the
/// earliest forecast day in the window is the baseline so the choice does
/// not depend on query row order.
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
    forecasts: &[Forecast],
    observations: &[Observation],
) -> Vec<Reading> {
    let Some(forecast) = forecasts
        .iter()
        .filter(|forecast| forecast.station_id == target)
        .min_by(|a, b| a.date.cmp(&b.date))
    else {
        return vec![];
    };
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
    let whole = |value: Option<i64>| value.map(|value| value as f64);
    vec![
        reading(
            TEMP_HIGH,
            Some(forecast.temp_high as f64),
            observed(|o| Some(o.temp_high)),
        ),
        reading(
            TEMP_LOW,
            Some(forecast.temp_low as f64),
            observed(|o| Some(o.temp_low)),
        ),
        reading(
            WIND_SPEED,
            whole(forecast.wind_speed),
            observed(|o| Some(o.wind_speed as f64)),
        ),
        reading(
            WIND_DIRECTION,
            whole(forecast.wind_direction),
            observed(|o| o.wind_direction.map(|value| value as f64)),
        ),
        reading(RAIN_AMT, forecast.rain_amt, observed(|o| o.rain_amt)),
        reading(SNOW_AMT, forecast.snow_amt, observed(|o| o.snow_amt)),
        reading(
            HUMIDITY,
            whole(forecast.humidity_max),
            observed(|o| o.humidity.map(|value| value as f64)),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn earliest_forecast_day_is_the_baseline_and_gaps_stay_missing() {
        let forecasts = vec![
            forecast("KORD", "2030-01-02", 55),
            forecast("KORD", "2030-01-01", 50),
            forecast("KSAW", "2030-01-01", 10),
        ];
        let readings = station_readings("KORD", &forecasts, &[]);
        let temp_high = readings.iter().find(|r| r.metric == TEMP_HIGH).unwrap();
        assert_eq!(temp_high.baseline, Some(50.0));
        assert_eq!(temp_high.observed, None);
        let wind = readings.iter().find(|r| r.metric == WIND_SPEED).unwrap();
        assert_eq!(wind.baseline, None, "a nil forecast is not calm");
        assert!(station_readings("KMSP", &forecasts, &[]).is_empty());
    }

    #[test]
    fn every_metric_has_a_reading() {
        let readings = station_readings("KORD", &[forecast("KORD", "2030-01-01", 50)], &[]);
        for metric in METRICS {
            assert!(readings.iter().any(|reading| reading.metric == metric.id));
        }
    }
}
