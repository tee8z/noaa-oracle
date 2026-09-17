use crate::TimeRange;
use anyhow::{Error, anyhow};
use serde::{Deserialize, Serialize};
use std::fmt::{self, Display};
use time::{OffsetDateTime, macros::format_description};

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(rename = "dwml")]
pub struct Dwml {
    #[serde(rename = "head")]
    pub head: Option<Head>,

    #[serde(rename = "data")]
    pub data: Data,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub struct Data {
    #[serde(rename = "location")]
    pub location: Vec<Location>,

    #[serde(rename = "time-layout")]
    pub time_layout: Vec<TimeLayout>,

    #[serde(rename = "parameters")]
    pub parameters: Vec<Parameter>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub struct Location {
    #[serde(rename = "location-key")]
    pub location_key: String,

    #[serde(rename = "point")]
    pub point: Point,

    // This is added after parsing to add in mapping the data further down the pipeline
    pub station_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Point {
    #[serde(rename = "@latitude")]
    pub latitude: String,

    #[serde(rename = "@longitude")]
    pub longitude: String,
}

impl Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{}", self.latitude, self.longitude)
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub struct Parameter {
    #[serde(rename = "temperature")]
    //holds max and min
    pub temperature: Option<Vec<DataReading>>,

    #[serde(rename = "precipitation")]
    // holds liquid (rain) and snow precipitation types
    pub precipitation: Option<Vec<DataReading>>,

    #[serde(rename = "wind-speed")]
    pub wind_speed: Option<DataReading>,

    #[serde(rename = "direction")]
    pub wind_direction: Option<DataReading>,

    #[serde(rename = "probability-of-precipitation")]
    pub probability_of_precipitation: Option<DataReading>,

    #[serde(rename = "humidity")]
    // holds max and min
    pub humidity: Option<Vec<DataReading>>,

    #[serde(rename = "winter-weather-outlook")]
    // holds snow ratio (ratio of snow accumulation to liquid equivalent)
    pub winter_weather_outlook: Option<DataReading>,

    #[serde(rename = "@applicable-location")]
    pub applicable_location: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub struct DataReading {
    #[serde(rename = "name", deserialize_with = "text_enum")]
    pub name: Name,

    #[serde(rename = "value")]
    pub value: Vec<String>,

    #[serde(rename = "@type")]
    pub reading_type: Type,

    #[serde(rename = "@units")]
    pub units: Units,

    #[serde(rename = "@time-layout")]
    pub time_layout: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Clone, Default)]
pub struct TimeLayout {
    #[serde(rename = "@time-coordinate")]
    pub time_coordinate: String,
    #[serde(rename = "@summarization")]
    pub summarization: Option<String>,
    /// Interleaved `layout-key`, `start-valid-time`, and `end-valid-time`
    /// children in document order.
    #[serde(rename = "#content")]
    pub time: Vec<Time>,
}
impl TimeLayout {
    pub fn to_time_ranges(&self) -> Result<Vec<TimeRange>, Error> {
        let description = format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second][offset_hour]:[offset_minute]"
        );
        let mut result =
            self.time
                .iter()
                .fold(vec![], |mut time_ranges: Vec<TimeRange>, current_time| {
                    match current_time {
                        Time::LayoutKey(key) => time_ranges.push(TimeRange {
                            key: key.to_string(),
                            start_time: OffsetDateTime::UNIX_EPOCH,
                            end_time: None,
                        }),
                        Time::StartTime(start_time) => {
                            let current_time = OffsetDateTime::parse(start_time, description)
                                .map_err(|e| anyhow!("error parsing time start time: {}", e))
                                .unwrap();
                            let previous = time_ranges.last().unwrap();
                            time_ranges.push(TimeRange {
                                key: previous.key.clone(),
                                start_time: current_time,
                                end_time: None,
                            })
                        }
                        Time::EndTime(end_time) => {
                            let current_time = OffsetDateTime::parse(end_time, description)
                                .map_err(|e| anyhow!("error parsing end time: {}", e))
                                .unwrap();
                            let previous = time_ranges.last_mut().unwrap();
                            previous.end_time = Some(current_time);
                        }
                    }
                    time_ranges
                });
        result.retain(|time_range| time_range.start_time != OffsetDateTime::UNIX_EPOCH);
        Ok(result.clone())
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub enum Time {
    #[serde(rename = "layout-key")]
    LayoutKey(String),
    #[serde(rename = "start-valid-time")]
    StartTime(String),
    #[serde(rename = "end-valid-time")]
    EndTime(String),
}

/// Deserializes a unit enum from an element's text, such as
/// `<name>Wind Speed</name>`. serde-xml-rs presents such an element as a
/// map with a `#text` key, which would otherwise be read as a variant name.
fn text_enum<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    #[derive(Deserialize)]
    struct Text {
        #[serde(rename = "#text")]
        text: String,
    }
    let Text { text } = Text::deserialize(deserializer)?;
    T::deserialize(serde::de::value::StringDeserializer::<D::Error>::new(text))
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub struct Head {
    #[serde(rename = "product")]
    pub product: Option<Product>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub struct Product {
    #[serde(rename = "creation-date")]
    pub creation_date: Option<CreationDate>,
}

/// `<creation-date refresh-frequency="PT30M">2026-09-17T00:12:38Z</creation-date>`
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub struct CreationDate {
    #[serde(rename = "@refresh-frequency")]
    pub refresh_frequency: Option<String>,
    #[serde(rename = "#text")]
    pub value: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub enum Type {
    #[serde(rename = "liquid")]
    Liquid,

    #[serde(rename = "maximum")]
    #[default]
    Maximum,

    #[serde(rename = "maximum relative")]
    MaximumRelative,

    #[serde(rename = "minimum")]
    Minimum,

    #[serde(rename = "minimum relative")]
    MinimumRelative,

    #[serde(rename = "sustained")]
    Sustained,

    #[serde(rename = "12 hour")]
    ProbabilityOfPrecipitationWithin12Hours,

    #[serde(rename = "wind")]
    Wind,

    #[serde(rename = "snow")]
    Snow,

    #[serde(rename = "ice")]
    Ice,

    #[serde(rename = "ratio of snow accumulation to its melted liquid equivalent")]
    SnowRatio,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub enum Name {
    #[serde(rename = "Daily Maximum Relative Humidity")]
    #[default]
    DailyMaximumRelativeHumidity,

    #[serde(rename = "Daily Maximum Temperature")]
    DailyMaximumTemperature,

    #[serde(rename = "Daily Minimum Relative Humidity")]
    DailyMinimumRelativeHumidity,

    #[serde(rename = "Daily Minimum Temperature")]
    DailyMinimumTemperature,

    #[serde(rename = "Liquid Precipitation Amount")]
    LiquidPrecipitationAmount,

    #[serde(rename = "Snow Amount")]
    SnowAmount,

    #[serde(rename = "Snow Ratio")]
    SnowRatio,

    #[serde(rename = "Ice Accumulation")]
    IceAccumulation,

    #[serde(rename = "12 Hourly Probability of Precipitation")]
    The12HourlyProbabilityOfPrecipitation,

    #[serde(rename = "Wind Direction")]
    WindDirection,

    #[serde(rename = "Wind Speed")]
    WindSpeed,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub enum Units {
    #[serde(rename = "degrees true")]
    DegreesTrue,

    #[serde(rename = "Fahrenheit")]
    Fahrenheit,

    #[serde(rename = "Celcius")]
    #[default]
    Celcius,

    #[serde(rename = "inches")]
    Inches,

    #[serde(rename = "knots")]
    Knots,

    #[serde(rename = "percent")]
    Percent,
}

impl Display for Units {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Units::DegreesTrue => write!(f, "degrees true"),
            Units::Fahrenheit => write!(f, "fahrenheit"),
            Units::Celcius => write!(f, "celcius"),
            Units::Inches => write!(f, "inches"),
            Units::Knots => write!(f, "knots"),
            Units::Percent => write!(f, "percent"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_xml;

    const DWML: &str = include_str!("testdata/dwml.xml");

    #[test]
    fn parses_noaa_time_series_with_interleaved_parameters() {
        let dwml: Dwml = parse_xml(DWML).unwrap();
        let creation = dwml.head.unwrap().product.unwrap().creation_date.unwrap();
        assert_eq!(creation.value, "2026-09-17T00:12:38Z");
        assert_eq!(creation.refresh_frequency.as_deref(), Some("PT30M"));

        assert_eq!(dwml.data.location.len(), 2);
        assert_eq!(dwml.data.location[0].location_key, "point1");
        assert_eq!(dwml.data.location[0].point.latitude, "41.98");
        assert_eq!(dwml.data.location[0].point.longitude, "-87.90");

        let layout = &dwml.data.time_layout[0];
        assert_eq!(layout.time_coordinate, "local");
        assert_eq!(layout.summarization.as_deref(), Some("none"));
        assert!(matches!(layout.time[0], Time::LayoutKey(_)));
        let ranges = layout.to_time_ranges().unwrap();
        assert!(!ranges.is_empty());
        assert!(ranges.iter().all(|range| range.end_time.is_some()));

        let parameters = &dwml.data.parameters[0];
        assert_eq!(parameters.applicable_location, "point1");
        let temperatures = parameters.temperature.as_ref().unwrap();
        assert_eq!(temperatures.len(), 2);
        assert_eq!(temperatures[0].reading_type, Type::Maximum);
        assert_eq!(temperatures[0].units, Units::Fahrenheit);
        assert_eq!(temperatures[0].name, Name::DailyMaximumTemperature);
        assert!(!temperatures[0].value.is_empty());
        assert!(temperatures[0].time_layout.starts_with("k-p24h"));
        // liquid, ice, and snow are separated by other elements in the document
        let precipitation = parameters.precipitation.as_ref().unwrap();
        assert_eq!(precipitation.len(), 3);
        assert_eq!(precipitation[0].reading_type, Type::Liquid);
        assert_eq!(precipitation[1].reading_type, Type::Ice);
        assert_eq!(precipitation[2].reading_type, Type::Snow);
        assert_eq!(parameters.wind_speed.as_ref().unwrap().units, Units::Knots);
        assert_eq!(
            parameters.humidity.as_ref().unwrap()[1].reading_type,
            Type::MinimumRelative
        );
    }
}
