//! A station's forecast detail: today's and the past week's forecasts against
//! what was observed, and the coming days, in the reader's calendar days (see
//! [`super::local_day`]). Built when a reader opens a station, or ahead of
//! time for the default airports in UTC days, and cached per calendar and
//! day for known stations.
//! A query that fails or runs too long is logged and not cached; the reader
//! gets an error with a retry instead of an empty table.

use super::QuerySchedule;
use std::{sync::Arc, time::Duration as StdDuration};

use futures::stream::{self, StreamExt};
use log::{error, info};
use time::{Date, Duration, OffsetDateTime};

use crate::{
    AppState, ForecastRequest, ObservationRequest, TemperatureUnit,
    cache::Cached,
    calendar::Calendar,
    heavy::release_freed_memory_now_and_then,
    templates::fragments::{ForecastComparison, ForecastDisplay, forecast_detail},
    weather_data::{self, DailyObservation, Forecast},
};

/// How long a reader waits for a forecast detail before getting an error
/// with a retry. htmx itself gives up after 10 s.
const FORECAST_TIMEOUT: StdDuration = StdDuration::from_secs(5);

/// One background value and one of its queries at a time. Public cache misses
/// retain their existing parallel path and share the query slots.
const WARM_CONCURRENCY: usize = 1;

/// Calendars the default airports' forecast details are built in ahead of
/// time: UTC and the newest readers' time zones.
const WARM_CALENDARS: usize = 3;

#[derive(Debug, thiserror::Error)]
pub(super) enum ForecastError {
    #[error(transparent)]
    Data(#[from] weather_data::Error),
    #[error("timed out after {} s", FORECAST_TIMEOUT.as_secs())]
    TimedOut,
}

/// The station's forecast detail, from the cache or built now. A stale
/// copy is served, and rebuilt in the background if a heavy turn is free
/// (see [`crate::heavy`]). Only successful builds for stations in the data
/// are cached, so neither an error nor an arbitrary path fills the cache.
pub(super) async fn forecast_html(
    state: &Arc<AppState>,
    station_id: &str,
    calendar: Calendar,
) -> Result<String, ForecastError> {
    let key = cache_key(station_id, calendar, OffsetDateTime::now_utc());
    match state.cached_forecast(&key) {
        Cached::Fresh(html) => return Ok(html),
        Cached::Stale { value, refresh } => {
            if refresh {
                refresh_forecast(state, key, station_id, calendar);
            }
            return Ok(value);
        }
        Cached::Missing => {}
    }
    let generation = state.data_generation();
    let built = match tokio::time::timeout(
        FORECAST_TIMEOUT,
        build(state, station_id, calendar, QuerySchedule::Parallel),
    )
    .await
    {
        Ok(built) => built.map_err(ForecastError::from),
        Err(_) => Err(ForecastError::TimedOut),
    };
    let html = built.inspect_err(|error| error!("forecast detail for {station_id}: {error}"))?;
    if state.is_known_station(station_id).await {
        state.cache_forecast(key, html.clone(), generation);
    }
    Ok(html)
}

/// Rebuilds a stale detail a reader found, in the background, on a heavy
/// turn free now; without one the next reader tries again.
fn refresh_forecast(state: &Arc<AppState>, key: String, station_id: &str, calendar: Calendar) {
    let Some(turn) = state.heavy().try_turn() else {
        state.forecast_refresh_failed(&key);
        return;
    };
    let (task_state, station_id) = (state.clone(), station_id.to_owned());
    state.spawn(async move {
        let _turn = turn;
        // Warming may have rebuilt it since the reader looked.
        if matches!(task_state.cached_forecast(&key), Cached::Fresh(_)) {
            return;
        }
        let generation = task_state.data_generation();
        match build(&task_state, &station_id, calendar, QuerySchedule::Serial).await {
            Ok(html) => task_state.cache_forecast(key, html, generation),
            Err(error) => {
                error!("refreshing the forecast detail for {station_id}: {error}");
                task_state.forecast_refresh_failed(&key);
            }
        }
    });
}

/// A station's detail differs by calendar and changes at the reader's
/// midnight.
fn cache_key(station_id: &str, calendar: Calendar, now: OffsetDateTime) -> String {
    format!("{station_id}@{}@{}", calendar.name(), calendar.date_of(now))
}

/// Builds the current weather readers asked for lately, and the default
/// airports' forecast details, in UTC days and in the calendars of those
/// readers. Called at startup, after new data and every 30 minutes;
/// readers get the values built before until each is replaced. A value
/// whose query fails keeps its old copy. Each value takes a heavy turn
/// (see [`crate::heavy`]), so a processing pass waits for at most the
/// values being built, and warming waits for the pass.
pub async fn warm_caches(state: &Arc<AppState>) {
    let started = std::time::Instant::now();
    let generation = state.data_generation();
    let mut calendars = vec![Calendar::Utc];
    let recent = state.recent_weather();
    for key in &recent {
        if !calendars.contains(&key.calendar) && calendars.len() < WARM_CALENDARS {
            calendars.push(key.calendar);
        }
    }
    // The default airports in UTC days are what a first visit shows.
    let default = super::weather::WeatherKey::new(
        &super::weather::default_airports(),
        None,
        None,
        Calendar::Utc,
        OffsetDateTime::now_utc(),
    );
    // Recent views move on to today, so a reader's first visit after their
    // midnight is ready too.
    let now = OffsetDateTime::now_utc();
    let mut weather = vec![];
    for key in recent
        .iter()
        .filter_map(|key| key.as_of(now))
        .chain([default])
    {
        if !weather.contains(&key) {
            weather.push(key);
        }
    }
    let rows = weather.len();
    stream::iter(weather)
        .for_each_concurrent(WARM_CONCURRENCY, |key| async move {
            let Some(_turn) = state.background_turn().await else {
                return;
            };
            super::weather::refresh_weather(state, key).await;
            release_freed_memory_now_and_then();
        })
        .await;
    let airports = super::weather::default_airports();
    let details = airports.len() * calendars.len();
    let jobs: Vec<_> = calendars
        .into_iter()
        .flat_map(|calendar| {
            airports
                .iter()
                .map(move |station| (station.clone(), calendar))
        })
        .collect();
    stream::iter(jobs)
        .for_each_concurrent(WARM_CONCURRENCY, |(station_id, calendar)| async move {
            let Some(_turn) = state.background_turn().await else {
                return;
            };
            match build(state, &station_id, calendar, QuerySchedule::Serial).await {
                Ok(html) => {
                    let key = cache_key(&station_id, calendar, OffsetDateTime::now_utc());
                    state.cache_forecast(key, html, generation)
                }
                Err(error) => error!("warming the forecast detail for {station_id}: {error}"),
            }
            release_freed_memory_now_and_then();
        })
        .await;
    info!(
        "warmed {rows} weather views and {details} forecast details in {:.1}s",
        started.elapsed().as_secs_f64()
    );
}

async fn build(
    state: &Arc<AppState>,
    station_id: &str,
    calendar: Calendar,
    schedule: QuerySchedule,
) -> Result<String, weather_data::Error> {
    // Forecasts and comparison observations use complete calendar days.
    let now = OffsetDateTime::now_utc();
    let today = calendar.date_of(now);
    let (coming, past) = schedule
        .try_join(
            coming_days(state, station_id, today, calendar),
            past_week(state, station_id, today, now, calendar, schedule),
        )
        .await?;
    Ok(forecast_detail(station_id, &past, &coming, &calendar.place()).into_string())
}

/// The latest forecast for the seven days after today, oldest first. Today's
/// forecast sits in the past-week table's "Today so far" row, so it is not
/// repeated here.
async fn coming_days(
    state: &Arc<AppState>,
    station_id: &str,
    today: Date,
    calendar: Calendar,
) -> Result<Vec<ForecastDisplay>, weather_data::Error> {
    let tomorrow = today + Duration::days(1);
    let request = ForecastRequest {
        start: Some(calendar.start_of(tomorrow)),
        end: Some(calendar.start_of(tomorrow + Duration::days(7))),
        generated_start: None,
        generated_end: None,
        station_ids: station_id.to_string(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let forecasts = state
        .weather_db
        .calendar_forecasts(&request, vec![station_id.to_string()], calendar)
        .await?;
    let tomorrow = tomorrow.to_string();
    let mut days: Vec<_> = forecasts
        .into_iter()
        .filter(|forecast| day(&forecast.date).is_some_and(|date| date >= tomorrow.as_str()))
        .map(display)
        .collect();
    days.sort_by(|a, b| a.date.cmp(&b.date));
    Ok(days)
}

/// Today so far and the seven complete days before it: each period's latest
/// retained forecast against calendar observations. Newest first.
async fn past_week(
    state: &Arc<AppState>,
    station_id: &str,
    today: Date,
    now: OffsetDateTime,
    calendar: Calendar,
    schedule: QuerySchedule,
) -> Result<Vec<ForecastComparison>, weather_data::Error> {
    let first = today - Duration::days(7);
    let (start, end) = (
        calendar.start_of(first),
        calendar.start_of(today + Duration::days(1)),
    );
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
        end: Some(now.min(end - Duration::nanoseconds(1))),
        station_ids: station_id.to_string(),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let stations = vec![station_id.to_string()];
    let (forecasts, observed) = schedule
        .try_join(
            state
                .weather_db
                .calendar_forecasts(&forecasts, stations.clone(), calendar),
            state
                .weather_db
                .calendar_daily_observations(&observations, stations, calendar),
        )
        .await?;
    let (start, today) = (first.to_string(), today.to_string());
    let mut days: Vec<_> = forecasts
        .into_iter()
        .filter(|forecast| {
            day(&forecast.date).is_some_and(|date| date >= start.as_str() && date <= today.as_str())
        })
        .map(|forecast| {
            let observed = observed
                .iter()
                .find(|observed| day(&observed.date) == day(&forecast.date));
            let so_far = day(&forecast.date) == Some(today.as_str());
            comparison(forecast, observed, so_far)
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

fn comparison(
    forecast: Forecast,
    observed: Option<&DailyObservation>,
    so_far: bool,
) -> ForecastComparison {
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
        so_far,
    }
}
