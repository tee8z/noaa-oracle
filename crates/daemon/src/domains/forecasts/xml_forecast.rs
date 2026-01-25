use crate::TimeRange;
use anyhow::{anyhow, Error};
use serde::{Deserialize, Serialize};
use std::fmt::{self, Display};
use time::{macros::format_description, OffsetDateTime};

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
    #[serde(rename = "latitude")]
    pub latitude: String,

    #[serde(rename = "longitude")]
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

    #[serde(rename = "applicable-location")]
    pub applicable_location: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub struct DataReading {
    #[serde(rename = "name")]
    pub name: Name,

    #[serde(rename = "value")]
    pub value: Vec<String>,

    #[serde(rename = "type")]
    pub reading_type: Type,

    #[serde(rename = "units")]
    pub units: Units,

    #[serde(rename = "time-layout")]
    pub time_layout: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Clone, Default)]
#[serde(rename_all = "PascalCase")]
pub struct TimeLayout {
    #[serde(rename = "time-coordinate")]
    pub time_coordinate: String,
    pub summarization: Option<String>,
    #[serde(rename = "$value")]
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

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub struct Head {
    #[serde(rename = "product")]
    pub product: Option<Product>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
pub struct Product {
    #[serde(rename = "creation-date")]
    pub creation_date: Option<String>,
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
