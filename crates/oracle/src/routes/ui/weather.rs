use std::{collections::HashMap, sync::Arc};

use time::{Duration, OffsetDateTime, Time, UtcOffset, format_description::well_known::Rfc3339};

use crate::{
    AppState, ForecastRequest, ObservationRequest, TemperatureUnit,
    templates::fragments::{ObservationPeriod, WeatherDisplay},
};

/// Refresh the requested selection, including stations without observations.
pub(super) fn refresh_path(
    station_ids: &[String],
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
) -> String {
    let mut parameters = Vec::new();
    if !station_ids.is_empty() {
        parameters.push(format!(
            "stations={}",
            encode_query_value(&station_ids.join(","))
        ));
    }
    for (name, value) in [("start", start), ("end", end)] {
        if let Some(value) = value {
            let value = value
                .to_offset(UtcOffset::UTC)
                .format(&Rfc3339)
                .unwrap_or_default();
            parameters.push(format!("{}={}", name, encode_query_value(&value)));
        }
    }
    if parameters.is_empty() {
        "/fragments/weather".into()
    } else {
        format!("/fragments/weather?{}", parameters.join("&"))
    }
}

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Use the same observation period and forecast vintage on initial load and refresh.
pub(super) async fn load_weather(
    state: &Arc<AppState>,
    station_ids: &[String],
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
) -> Vec<WeatherDisplay> {
    let now = OffsetDateTime::now_utc();
    let selected = start.is_some() || end.is_some();
    let start = start
        .unwrap_or_else(|| now.replace_time(Time::MIDNIGHT))
        .to_offset(UtcOffset::UTC);
    let end = end.unwrap_or(now).to_offset(UtcOffset::UTC);
    let day_start = start.replace_time(Time::MIDNIGHT);
    let day_end = day_start.saturating_add(Duration::days(1));
    let single_day = start <= end && end <= day_end;
    let period = if selected {
        ObservationPeriod::Selected {
            start: start.format(&Rfc3339).unwrap_or_default(),
            end: end.format(&Rfc3339).unwrap_or_default(),
        }
    } else {
        ObservationPeriod::Today
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
                state
                    .weather_db
                    .observation_data(&recent_request, station_ids.to_vec())
                    .await
                    .ok()
            } else {
                None
            }
        },
        async {
            if single_day {
                state
                    .weather_db
                    .forecasts_data(&forecast_request, station_ids.to_vec())
                    .await
                    .unwrap_or_default()
            } else {
                vec![]
            }
        },
        state.stations(),
    );
    let observations = observations.unwrap_or_default();
    let stations = stations.unwrap_or_else(|error| {
        log::error!("failed to read stations: {error:#}");
        Default::default()
    });
    let period_by_station: HashMap<_, _> = observations
        .iter()
        .map(|obs| (&obs.station_id, obs))
        .collect();
    let date = day_start.date().to_string();
    let forecast_by_station: HashMap<_, _> = forecasts
        .iter()
        .filter(|forecast| forecast.date.get(..10) == Some(date.as_str()))
        .map(|forecast| (&forecast.station_id, forecast))
        .collect();

    // Keep the latest report visible across midnight, even before today's
    // first observation. Period summaries remain empty until today's data arrives.
    recent
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
                elevation_m: station.and_then(|station| station.elevation_m),
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
        .collect()
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
    fn query_values_cannot_introduce_other_parameters() {
        assert_eq!(
            refresh_path(&["KPWM&start=other".into()], None, None),
            "/fragments/weather?stations=KPWM%26start%3Dother"
        );
    }
}
