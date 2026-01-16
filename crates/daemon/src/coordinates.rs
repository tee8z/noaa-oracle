use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use crate::{Point, XmlFetcher};
use anyhow::Error;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WeatherStation {
    pub station_id: String,
    pub station_name: String,
    pub state: String,
    pub iata_id: String,
    pub elevation_m: Option<f64>,
    pub latitude: String,
    pub longitude: String,
}
impl fmt::Display for WeatherStation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Station ID: {}, Station Name: {}, State: {}, IATA: {}, Elevation: {:?}m, Latitude: {}, Longitude: {}",
            self.station_id, self.station_name, self.state, self.iata_id, self.elevation_m, self.latitude, self.longitude
        )
    }
}

impl Station {
    /// Try to convert to WeatherStation, returning None if latitude or longitude is missing or invalid
    fn try_into_weather_station(self) -> Option<WeatherStation> {
        let latitude = self.latitude?;
        let longitude = self.longitude?;

        // Skip invalid placeholder coordinates (-99.99, -99.99)
        let lat_val: f64 = latitude.parse().ok()?;
        let lon_val: f64 = longitude.parse().ok()?;
        if !(-90.0..=90.0).contains(&lat_val) || !(-180.0..=180.0).contains(&lon_val) {
            return None;
        }

        // Parse elevation if available
        let elevation_m = self.elevation_m.and_then(|e| e.parse::<f64>().ok());

        Some(WeatherStation {
            station_id: self.station_id,
            station_name: self.site.unwrap_or_default(),
            state: self.state.unwrap_or_default(),
            iata_id: self.iata_id.unwrap_or_default(),
            elevation_m,
            latitude,
            longitude,
        })
    }
}

impl WeatherStation {
    pub fn get_latitude(&self) -> String {
        format!("{:.2}", self.latitude.parse::<f64>().unwrap())
    }
    pub fn get_longitude(&self) -> String {
        format!("{:.2}", self.longitude.parse::<f64>().unwrap())
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CityWeather {
    #[serde(flatten)]
    pub city_data: HashMap<String, WeatherStation>,
}

impl fmt::Display for CityWeather {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (city, station) in &self.city_data {
            writeln!(
                f,
                "City: {}, Station ID: {}, Latitude: {}, Longitude: {}",
                city, station.station_id, station.latitude, station.longitude
            )?;
        }
        Ok(())
    }
}
impl CityWeather {
    pub fn get_coordinates_url(&self) -> String {
        self.get_coordinates().join("%20")
    }

    pub fn get_coordinates(&self) -> Vec<String> {
        self.city_data
            .values()
            .map(|weather_station| {
                format!(
                    "{},{}",
                    weather_station.get_latitude(),
                    weather_station.get_longitude()
                )
            })
            .collect::<Vec<String>>()
    }
    pub fn remove_coordinates(mut self, point: Point) {
        self.city_data
            .retain(|_, v| !(v.latitude == point.latitude && v.longitude == point.longitude));
    }
    pub fn get_station_ids(&self) -> HashSet<String> {
        let mut station_ids: HashSet<String> = HashSet::new();
        self.city_data.iter().for_each(|(_city_name, city_data)| {
            station_ids.insert(city_data.station_id.clone());
        });
        station_ids
    }
}

pub fn split_cityweather(original: CityWeather, max_keys_per_map: usize) -> Vec<CityWeather> {
    let mut result: Vec<CityWeather> = Vec::new();
    let mut current_map = HashMap::new();
    let mut current_keys = 0;

    for (key, value) in original.city_data {
        // Check if adding the entry exceeds the maximum keys
        if current_keys + 1 > max_keys_per_map {
            // If yes, start a new map
            result.push(CityWeather {
                city_data: std::mem::take(&mut current_map),
            });
            current_keys = 0;
        }

        // Add the entry to the current map
        current_map.insert(key.clone(), value.clone());
        current_keys += 1;
    }

    // Add the last map if not empty
    if !current_map.is_empty() {
        result.push(CityWeather {
            city_data: current_map,
        });
    }

    result
}

static STATE_ABBERVIATIONS: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID", "IL", "IN", "IA",
    "KS", "KY", "LA", "ME", "MT", "NE", "NV", "NH", "NJ", "NM", "NY", "NC", "ND", "OH", "OK", "OR",
    "MD", "MA", "MI", "MN", "MS", "MO", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA",
    "WV", "WI", "WY",
];

pub async fn get_coordinates(fetcher: Arc<XmlFetcher>) -> Result<CityWeather, Error> {
    let mut city_data: HashMap<String, WeatherStation> = HashMap::new();
    // Broken @ NOAA: https://forecast.weather.gov/xml/current_obs/index.xml

    let raw_xml = fetcher
        .fetch_xml_gzip("https://aviationweather.gov/data/cache/stations.cache.xml.gz")
        .await?;
    let converted_xml: WxStationIndex = serde_xml_rs::from_str(&raw_xml)?;

    for station in converted_xml.data.station {
        // Skip any place not in the US
        if let Some(country) = &station.country {
            if country != "US" {
                continue;
            }
        }
        if let Some(state) = &station.state {
            if !STATE_ABBERVIATIONS.contains(&state.as_str()) {
                continue;
            }
        } else {
            continue;
        }
        let station_id = station.station_id.clone();
        if let Some(weather_station) = station.try_into_weather_station() {
            city_data.insert(station_id, weather_station);
        }
    }

    Ok(CityWeather { city_data })
}

#[derive(Serialize, Deserialize)]
#[serde(rename = "response")]
pub struct WxStationIndex {
    #[serde(rename = "request_index")]
    request_index: String,

    #[serde(rename = "data_source")]
    data_source: DataSource,

    #[serde(rename = "request")]
    request: Request,

    #[serde(rename = "errors")]
    errors: String,

    #[serde(rename = "warnings")]
    warnings: String,

    #[serde(rename = "time_taken_ms")]
    time_taken_ms: Option<String>,

    #[serde(rename = "data")]
    data: StationData,
}

#[derive(Serialize, Deserialize)]
pub struct StationData {
    #[serde(rename = "Station")]
    station: Vec<Station>,

    // num_results is now an attribute on the data element, not a child element
    #[serde(rename = "num_results", default)]
    num_results: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Station {
    #[serde(rename = "station_id")]
    station_id: String,

    #[serde(rename = "latitude")]
    latitude: Option<String>,

    #[serde(rename = "longitude")]
    longitude: Option<String>,

    #[serde(rename = "elevation_m")]
    elevation_m: Option<String>,

    #[serde(rename = "site")]
    site: Option<String>,

    #[serde(rename = "country")]
    country: Option<String>,

    #[serde(rename = "wmo_id")]
    wmo_id: Option<String>,

    #[serde(rename = "state")]
    state: Option<String>,

    #[serde(rename = "faa_id")]
    faa_id: Option<String>,

    // site_type now contains nested METAR/TAF elements indicating available data types
    #[serde(rename = "site_type")]
    site_type: Option<SiteType>,

    #[serde(rename = "icao_id")]
    icao_id: Option<String>,

    #[serde(rename = "iata_id")]
    iata_id: Option<String>,
}

/// Represents the site_type element which can contain METAR and TAF child elements
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct SiteType {
    #[serde(rename = "METAR", default)]
    metar: Option<EmptyElement>,

    #[serde(rename = "TAF", default)]
    taf: Option<EmptyElement>,
}

/// Represents an empty XML element like <METAR/> or <TAF/>
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct EmptyElement;

#[derive(Serialize, Deserialize)]
pub struct DataSource {
    #[serde(rename = "name")]
    name: String,
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    #[serde(rename = "type")]
    request_type: String,
}
