use std::{collections::HashMap, sync::Arc};

use time::{Date, Duration, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

use crate::{
    AppState, ForecastRequest, ObservationRequest, TemperatureUnit,
    cache::Cached,
    calendar::Calendar,
    routes::stations::MAX_STATIONS,
    templates::fragments::{ObservationPeriod, WeatherDisplay, weather::with_parameters},
    weather_data::validate_station_id,
};

/// Stations shown when none are named: major airports covering every state.
pub(super) const DEFAULT_MAJOR_AIRPORTS: &[&str] = &[
    "KATL", "KLAX", "KORD", "KDFW", "KDEN", "KJFK", "KSFO", "KSEA", "KLAS", "KMCO", "KEWR", "KMIA",
    "KPHX", "KIAH", "KBOS", "KMSP", "KFLL", "KDTW", "KPHL", "KLGA", "KBWI", "KSLC", "KDCA", "KSAN",
    "KTPA", "KPDX", "KSTL", "KHNL", "KBNA", "KAUS", "KMCI", "KRDU", "KMKE", "KSMF", "KCLT", "KPIT",
    "KSAT", "KOAK", "KCLE", "KSJC", "KIND", "KCVG", "KCMH", "KJAN", "KRSW", "KABQ", "KANC", "KOMA",
    "KBUF", "KPBI", "KBDL", "KPVD", "KBTV", "KPWM", "KMHT", "KBOI", "KBIL", "KFSD", "KFAR", "KGEG",
    "KICT", "KLIT", "KLEX", "KBHM", "KMEM", "KJAX", "KCHS", "KRIC", "KORF", "KCRW", "KPNS", "KMOB",
    "KSHV", "KMSY", "KTUL", "KELP", "KTUS", "KCOS", "KGRR", "KDSM", "KMSN", "KDLH", "KBZN", "KGJT",
    "KRAP", "KFCA", "KCYS", "KJAR", "KSGF", "KFSM",
];

pub(super) fn default_airports() -> Vec<String> {
    DEFAULT_MAJOR_AIRPORTS
        .iter()
        .map(|station| station.to_string())
        .collect()
}

/// The stations named in `?stations=`: valid ids, each once, at most
/// [`MAX_STATIONS`]. `None` when none is usable, so `?stations=` shows the
/// default airports instead of querying every station.
pub(super) fn requested_stations(value: Option<&str>) -> Option<Vec<String>> {
    let mut ids: Vec<String> = vec![];
    for id in value?.split(',').map(str::trim) {
        if validate_station_id(id).is_ok() && !ids.iter().any(|known| known == id) {
            ids.push(id.to_string());
        }
    }
    ids.truncate(MAX_STATIONS);
    (!ids.is_empty()).then_some(ids)
}

/// Refresh the requested selection, including stations without observations.
pub(super) fn refresh_path(
    station_ids: &[String],
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
) -> String {
    let stations = station_ids.join(",");
    let time = |value: Option<OffsetDateTime>| {
        value.map(|value| {
            value
                .to_offset(UtcOffset::UTC)
                .format(&Rfc3339)
                .unwrap_or_default()
        })
    };
    let (start, end) = (time(start), time(end));
    let parameters: Vec<(&str, &str)> = [
        (
            "stations",
            (!stations.is_empty()).then_some(stations.as_str()),
        ),
        ("start", start.as_deref()),
        ("end", end.as_deref()),
    ]
    .into_iter()
    .filter_map(|(name, value)| Some((name, value?)))
    .collect();
    with_parameters("/fragments/weather", &parameters)
}

/// What current weather shows: which stations, over which period, in which
/// calendar's day. It changes only when new reports arrive, so it is
/// cached by this key (see [`load_weather`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WeatherKey {
    stations: Vec<String>,
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
    pub calendar: Calendar,
    /// The day it covers: a reader's today changes at their midnight.
    pub day: Date,
}

impl WeatherKey {
    /// A selected period keeps UTC days, as its address says.
    pub fn new(
        station_ids: &[String],
        start: Option<OffsetDateTime>,
        end: Option<OffsetDateTime>,
        calendar: Calendar,
        now: OffsetDateTime,
    ) -> Self {
        let calendar = if start.is_some() || end.is_some() {
            Calendar::Utc
        } else {
            calendar
        };
        Self {
            stations: station_ids.to_vec(),
            start,
            end,
            calendar,
            day: calendar.date_of(start.unwrap_or(now)),
        }
    }

    /// The same view as of `now`: without a selected period, a reader's
    /// "today" moves on at their midnight. `None` for a selected period,
    /// which the warmer leaves to readers.
    pub fn as_of(&self, now: OffsetDateTime) -> Option<Self> {
        (self.start.is_none() && self.end.is_none()).then(|| Self {
            day: self.calendar.date_of(now),
            ..self.clone()
        })
    }
}

/// Current weather for the stations and period, from the cache or built
/// now. A stale copy is served while it is rebuilt in the background; only
/// builds whose queries all succeeded are cached.
pub(super) async fn load_weather(
    state: &Arc<AppState>,
    station_ids: &[String],
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
    calendar: Calendar,
) -> Arc<Vec<WeatherDisplay>> {
    let key = WeatherKey::new(station_ids, start, end, calendar, OffsetDateTime::now_utc());
    match state.cached_weather(&key) {
        Cached::Fresh(weather) => weather,
        Cached::Stale { value, refresh } => {
            if refresh {
                let task_state = state.clone();
                state.spawn(async move { refresh_weather(&task_state, key).await });
            }
            value
        }
        Cached::Missing => {
            let generation = state.data_generation();
            let (weather, complete) = build_weather(state, station_ids, start, end, calendar).await;
            let weather = Arc::new(weather);
            if complete {
                state.cache_weather(key, weather.clone(), generation);
            }
            weather
        }
    }
}

/// Rebuilds and caches the current weather for `key`; a failed query
/// leaves the cached copy.
pub(super) async fn refresh_weather(state: &Arc<AppState>, key: WeatherKey) {
    let generation = state.data_generation();
    let (weather, complete) =
        build_weather(state, &key.stations, key.start, key.end, key.calendar).await;
    if complete {
        state.cache_weather(key, Arc::new(weather), generation);
    } else {
        state.weather_refresh_failed(&key);
    }
}

/// Use the same observation period and forecast vintage on initial load and refresh.
/// Without a selected period, today is the reader's day in `calendar`
/// (see [`super::local_day`]); a selected period keeps UTC days, as its
/// address says. Also says whether every query succeeded.
async fn build_weather(
    state: &Arc<AppState>,
    station_ids: &[String],
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
    calendar: Calendar,
) -> (Vec<WeatherDisplay>, bool) {
    let now = OffsetDateTime::now_utc();
    let selected = start.is_some() || end.is_some();
    let calendar = if selected { Calendar::Utc } else { calendar };
    let start = start
        .unwrap_or_else(|| calendar.start_of_day(now))
        .to_offset(UtcOffset::UTC);
    let end = end.unwrap_or(now).to_offset(UtcOffset::UTC);
    let day = calendar.date_of(start);
    let day_start = calendar.start_of(day);
    // Days with a daylight-saving change last 23 or 25 hours.
    let day_end = day
        .next_day()
        .map(|next| calendar.start_of(next))
        .unwrap_or_else(|| day_start.saturating_add(Duration::days(1)));
    let single_day = start <= end && end <= day_end;
    let period = if selected {
        ObservationPeriod::Selected {
            start: start.format(&Rfc3339).unwrap_or_default(),
            end: end.format(&Rfc3339).unwrap_or_default(),
        }
    } else {
        ObservationPeriod::Today {
            zone: calendar.place(),
        }
    };
    let request = ObservationRequest {
        start: Some(start),
        // An explicit full calendar day ends just before the next midnight.
        end: Some(if end == day_end {
            end.saturating_sub(Duration::nanoseconds(1))
        } else {
            end
        }),
        station_ids: station_ids.join(","),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let recent_request = ObservationRequest {
        start: Some(now.saturating_sub(Duration::hours(24))),
        end: Some(now),
        station_ids: station_ids.join(","),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };
    let forecast_request = ForecastRequest {
        start: Some(day_start),
        end: Some(day_end),
        generated_start: Some(day_start.saturating_sub(Duration::days(1))),
        generated_end: Some(day_start.saturating_sub(Duration::nanoseconds(1))),
        station_ids: station_ids.join(","),
        temperature_unit: TemperatureUnit::Fahrenheit,
    };

    let (observations, recent, forecasts, stations) = tokio::join!(
        state
            .weather_db
            .observation_data(&request, station_ids.to_vec()),
        async {
            if !selected {
                Some(
                    state
                        .weather_db
                        .observation_data(&recent_request, station_ids.to_vec())
                        .await,
                )
            } else {
                None
            }
        },
        async {
            if single_day {
                state
                    .weather_db
                    .calendar_forecasts(&forecast_request, station_ids.to_vec(), calendar)
                    .await
            } else {
                Ok(vec![])
            }
        },
        state.stations(),
    );
    let mut complete = true;
    let observations = observations.unwrap_or_else(|error| {
        log::error!("failed to read observations for the weather table: {error:#}");
        complete = false;
        vec![]
    });
    let recent = recent.and_then(|recent| {
        recent
            .inspect_err(|error| {
                log::error!("failed to read the latest reports for the weather table: {error:#}");
                complete = false;
            })
            .ok()
    });
    let forecasts = forecasts.unwrap_or_else(|error| {
        log::error!("failed to read forecasts for the weather table: {error:#}");
        complete = false;
        vec![]
    });
    let stations = stations.unwrap_or_else(|error| {
        log::error!("failed to read stations: {error:#}");
        complete = false;
        Default::default()
    });
    let period_by_station: HashMap<_, _> = observations
        .iter()
        .map(|obs| (&obs.station_id, obs))
        .collect();
    let date = day.to_string();
    let forecast_by_station: HashMap<_, _> = forecasts
        .iter()
        .filter(|forecast| forecast.date.get(..10) == Some(date.as_str()))
        .map(|forecast| (&forecast.station_id, forecast))
        .collect();

    // Keep the latest report visible across midnight, even before today's
    // first observation. Period summaries remain empty until today's data arrives.
    let weather = recent
        .as_deref()
        .unwrap_or(&observations)
        .iter()
        .map(|latest| {
            let obs = period_by_station.get(&latest.station_id).copied();
            let station = stations
                .iter()
                .find(|station| station.station_id == latest.station_id);
            let forecast = forecast_by_station.get(&latest.station_id);
            WeatherDisplay {
                station_id: latest.station_id.clone(),
                station_name: station
                    .map(|station| station.station_name.clone())
                    .unwrap_or_default(),
                state: station
                    .map(|station| station.state.clone())
                    .unwrap_or_default(),
                iata_id: station
                    .map(|station| station.iata_id.clone())
                    .unwrap_or_default(),
                latest_temp: latest.latest_temp,
                latest_temp_time: latest.latest_temp_time.clone(),
                observation_period: period.clone(),
                temp_high: obs.map(|obs| obs.temp_high),
                temp_low: obs.map(|obs| obs.temp_low),
                wind_speed: obs.and_then(|obs| obs.wind_speed),
                wind_direction: obs.and_then(|obs| obs.wind_direction),
                humidity: obs.and_then(|obs| obs.humidity),
                rain_amt: obs.and_then(|obs| obs.rain_amt),
                snow_amt: obs.and_then(|obs| obs.snow_amt),
                observed_start: obs.map(|obs| obs.start_time.clone()).unwrap_or_default(),
                observed_end: obs.map(|obs| obs.end_time.clone()).unwrap_or_default(),
                latitude: station.map(|station| station.latitude).unwrap_or(0.0),
                longitude: station.map(|station| station.longitude).unwrap_or(0.0),
                forecast_high: forecast.map(|forecast| forecast.temp_high),
                forecast_low: forecast.map(|forecast| forecast.temp_low),
            }
        })
        .collect();
    (weather, complete)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_path_preserves_selection_and_normalizes_offsets() {
        let start = OffsetDateTime::parse("2026-09-20T02:00:00+02:00", &Rfc3339).unwrap();
        let end = start + Duration::days(1);
        assert_eq!(
            refresh_path(&["KPWM".into(), "KBOS".into()], Some(start), Some(end)),
            "/fragments/weather?stations=KPWM%2CKBOS&start=2026-09-20T00%3A00%3A00Z&end=2026-09-21T00%3A00%3A00Z"
        );
        assert_eq!(refresh_path(&[], None, None), "/fragments/weather");
        assert_eq!(
            refresh_path(&[], Some(start), Some(end)),
            "/fragments/weather?start=2026-09-20T00%3A00%3A00Z&end=2026-09-21T00%3A00%3A00Z"
        );
    }

    #[test]
    fn only_usable_station_lists_replace_the_default_airports() {
        assert_eq!(requested_stations(None), None);
        assert_eq!(requested_stations(Some("")), None);
        assert_eq!(requested_stations(Some(" , ,")), None);
        assert_eq!(requested_stations(Some("bad'id")), None);
        assert_eq!(
            requested_stations(Some("KPWM, KBOS,KPWM,bad'id")),
            Some(vec!["KPWM".to_string(), "KBOS".to_string()])
        );
        let many = ["KORD"; 3].join(",")
            + ","
            + &(0..200)
                .map(|n| format!("K{n}"))
                .collect::<Vec<_>>()
                .join(",");
        assert_eq!(requested_stations(Some(&many)).unwrap().len(), MAX_STATIONS);
    }

    #[test]
    fn query_values_cannot_introduce_other_parameters() {
        assert_eq!(
            refresh_path(&["KPWM&start=other".into()], None, None),
            "/fragments/weather?stations=KPWM%26start%3Dother"
        );
    }
}
