//! A station's forecast detail: the past week's forecasts against what was
//! observed, and the coming days, in the reader's calendar days (see
//! [`super::local_day`]). Built when a reader opens a station, or ahead of
//! time for the default airports in UTC days, and cached per calendar and
//! day for known stations.
//! A query that fails or runs too long is logged and not cached; the reader
//! gets an error with a retry instead of an empty table.

use std::{sync::Arc, time::Duration as StdDuration};

use futures::stream::{self, StreamExt};
use log::{error, info};
use time::{Date, Duration, OffsetDateTime};

use crate::{
    AppState, ForecastRequest, ObservationRequest, TemperatureUnit,
    calendar::Calendar,
    templates::fragments::{ForecastComparison, ForecastDisplay, forecast_detail},
    weather_data::{self, DailyObservation, Forecast},
};

/// How long a reader waits for a forecast detail before getting an error
/// with a retry. htmx itself gives up after 10 s.
const FORECAST_TIMEOUT: StdDuration = StdDuration::from_secs(5);

/// Stations the cache warmer builds at once.
const WARM_CONCURRENCY: usize = 10;

#[derive(Debug, thiserror::Error)]
pub(super) enum ForecastError {
    #[error(transparent)]
    Data(#[from] weather_data::Error),
    #[error("timed out after {} s", FORECAST_TIMEOUT.as_secs())]
    TimedOut,
}

/// The station's forecast detail, from the cache or built now. Only
/// successful builds for stations in the data are cached, so neither an
/// error nor an arbitrary path fills the cache.
pub(super) async fn forecast_html(
    state: &Arc<AppState>,
    station_id: &str,
    calendar: Calendar,
) -> Result<String, ForecastError> {
    let key = cache_key(station_id, calendar, OffsetDateTime::now_utc());
    if let Some(cached) = state.cached_forecast(&key) {
        return Ok(cached);
    }
    let built =
        match tokio::time::timeout(FORECAST_TIMEOUT, build(state, station_id, calendar)).await {
            Ok(built) => built.map_err(ForecastError::from),
            Err(_) => Err(ForecastError::TimedOut),
        };
    let html = built.inspect_err(|error| error!("forecast detail for {station_id}: {error}"))?;
    if state.is_known_station(station_id).await {
        state.cache_forecast(key, html.clone());
    }
    Ok(html)
}

/// A station's detail differs by calendar and changes at the reader's
/// midnight.
fn cache_key(station_id: &str, calendar: Calendar, now: OffsetDateTime) -> String {
    format!("{station_id}@{}@{}", calendar.name(), calendar.date_of(now))
}

/// Pre-warms the forecast cache for the default airports. Called at startup
/// and every 30 minutes; a station whose query fails is left uncached.
pub async fn warm_forecast_cache(state: &Arc<AppState>) {
    let airports = super::weather::default_airports();
    info!("Warming forecast cache for {} stations...", airports.len());
    stream::iter(airports)
        .for_each_concurrent(WARM_CONCURRENCY, |station_id| async move {
            match build(state, &station_id, Calendar::Utc).await {
                Ok(html) => {
                    let key = cache_key(&station_id, Calendar::Utc, OffsetDateTime::now_utc());
                    state.cache_forecast(key, html)
                }
                Err(error) => error!("warming the forecast detail for {station_id}: {error}"),
            }
        })
        .await;
    info!("Forecast cache warming complete.");
}

async fn build(
    state: &Arc<AppState>,
    station_id: &str,
    calendar: Calendar,
) -> Result<String, weather_data::Error> {
    // Forecasts and comparison observations use complete calendar days.
    let now = OffsetDateTime::now_utc();
    let today = calendar.date_of(now);
    let (coming, past) = tokio::try_join!(
        coming_days(state, station_id, today, calendar),
        past_week(state, station_id, today, now, calendar)
    )?;
    Ok(forecast_detail(station_id, &past, &coming, &calendar.place()).into_string())
}

/// The latest forecast for today and the next six days, oldest first.
async fn coming_days(
    state: &Arc<AppState>,
    station_id: &str,
    today: Date,
    calendar: Calendar,
) -> Result<Vec<ForecastDisplay>, weather_data::Error> {
    let request = ForecastRequest {
        start: Some(calendar.start_of(today)),
        end: Some(calendar.start_of(today + Duration::days(7))),
        generated_start: None,
        generated_end: None,
        station_ids: station_id.to_string(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let forecasts = state
        .weather_db
        .calendar_forecasts(&request, vec![station_id.to_string()], calendar)
        .await?;
    let today = today.to_string();
    let mut days: Vec<_> = forecasts
        .into_iter()
        .filter(|forecast| day(&forecast.date).is_some_and(|date| date >= today.as_str()))
        .map(display)
        .collect();
    days.sort_by(|a, b| a.date.cmp(&b.date));
    Ok(days)
}

/// The seven complete days before today: each day's forecast, issued the
/// day before, against what was observed. Newest first.
async fn past_week(
    state: &Arc<AppState>,
    station_id: &str,
    today: Date,
    now: OffsetDateTime,
    calendar: Calendar,
) -> Result<Vec<ForecastComparison>, weather_data::Error> {
    let first = today - Duration::days(7);
    let (start, end) = (calendar.start_of(first), calendar.start_of(today));
    let forecasts = ForecastRequest {
        start: Some(start),
        end: Some(end),
        generated_start: Some(start - Duration::days(1)),
        generated_end: Some(now),
        station_ids: station_id.to_string(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let observations = ObservationRequest {
        start: Some(start),
        end: Some(end - Duration::nanoseconds(1)),
        station_ids: station_id.to_string(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let stations = vec![station_id.to_string()];
    let (forecasts, observed) = tokio::try_join!(
        state
            .weather_db
            .calendar_forecasts(&forecasts, stations.clone(), calendar),
        state
            .weather_db
            .calendar_daily_observations(&observations, stations, calendar)
    )?;
    let (start, today) = (first.to_string(), today.to_string());
    let mut days: Vec<_> = forecasts
        .into_iter()
        .filter(|forecast| {
            day(&forecast.date).is_some_and(|date| date >= start.as_str() && date < today.as_str())
        })
        .map(|forecast| {
            let observed = observed
                .iter()
                .find(|observed| day(&observed.date) == day(&forecast.date));
            comparison(forecast, observed)
        })
        .collect();
    days.sort_by(|a, b| b.date.cmp(&a.date));
    Ok(days)
}

/// The `YYYY-MM-DD` a forecast or observation date starts with.
fn day(date: &str) -> Option<&str> {
    date.get(..10)
}

fn display(forecast: Forecast) -> ForecastDisplay {
    ForecastDisplay {
        date: forecast.date,
        temp_high: forecast.temp_high,
        temp_low: forecast.temp_low,
        wind_speed: forecast.wind_speed,
        wind_direction: forecast.wind_direction,
        humidity_max: forecast.humidity_max,
        humidity_min: forecast.humidity_min,
        precip_chance: forecast.precip_chance,
        rain_amt: forecast.rain_amt,
        snow_amt: forecast.snow_amt,
    }
}

fn comparison(forecast: Forecast, observed: Option<&DailyObservation>) -> ForecastComparison {
    ForecastComparison {
        date: forecast.date,
        forecast_high: forecast.temp_high,
        forecast_low: forecast.temp_low,
        forecast_wind: forecast.wind_speed,
        forecast_humidity_max: forecast.humidity_max,
        forecast_humidity_min: forecast.humidity_min,
        forecast_rain: forecast.rain_amt,
        forecast_snow: forecast.snow_amt,
        actual_high: observed.map(|observed| observed.temp_high),
        actual_low: observed.map(|observed| observed.temp_low),
        actual_wind: observed.and_then(|observed| observed.wind_speed),
        actual_humidity: observed.and_then(|observed| observed.humidity),
        actual_rain: observed.and_then(|observed| observed.rain_amt),
        actual_snow: observed.and_then(|observed| observed.snow_amt),
    }
}
